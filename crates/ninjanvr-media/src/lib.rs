use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{NaiveDateTime, TimeZone, Utc};
use ninjanvr_config::NinjaConfig;
use ninjanvr_db::{NinjaDatabase, RecordingRow};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Resolves the `ffmpeg` binary path across container and host installations.
pub fn resolve_ffmpeg_bin() -> PathBuf {
    let mut candidates = Vec::new();
    if let Ok(env_bin) = std::env::var("NINJANVR_FFMPEG_BIN") {
        candidates.push(PathBuf::from(env_bin));
    }
    candidates.extend([
        PathBuf::from("/usr/lib/ffmpeg/8.0/bin/ffmpeg"),
        PathBuf::from("/usr/lib/ffmpeg/7.0/bin/ffmpeg"),
        PathBuf::from("/usr/lib/ffmpeg/6.0/bin/ffmpeg"),
        PathBuf::from("/usr/lib/ffmpeg/5.0/bin/ffmpeg"),
        PathBuf::from("/usr/local/bin/ffmpeg"),
        PathBuf::from("/usr/bin/ffmpeg"),
    ]);
    candidates
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

/// Represents a decoded YUV420p (I420) frame in memory, shared zero-copy via `Arc<YuvFrame>`.
#[derive(Debug, Clone)]
pub struct YuvFrame {
    pub camera: Arc<str>,
    pub timestamp: f64,
    pub width: u32,
    pub height: u32,
    pub data: Arc<Vec<u8>>,
}

impl YuvFrame {
    pub fn y_plane(&self) -> &[u8] {
        let y_len = (self.width as usize) * (self.height as usize);
        &self.data[..y_len.min(self.data.len())]
    }
}

/// Per-second activity sample stored in an in-memory ring buffer per camera
/// so closed recording segments know their exact motion & object counts immediately
/// without polling `/proc` or scanning external queues.
#[derive(Debug, Clone, Copy)]
pub struct ActivitySample {
    pub timestamp: f64,
    pub motion_px: u32,
    pub active_objects: u32,
    pub regions: u32,
}

#[derive(Default)]
pub struct CameraActivityRing {
    samples: VecDeque<ActivitySample>,
}

impl CameraActivityRing {
    pub fn record_sample(
        &mut self,
        timestamp: f64,
        motion_px: u32,
        active_objects: u32,
        regions: u32,
    ) {
        self.samples.push_back(ActivitySample {
            timestamp,
            motion_px,
            active_objects,
            regions,
        });
        let cutoff = timestamp - 300.0;
        while let Some(front) = self.samples.front() {
            if front.timestamp < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn aggregate_range(&self, start_time: f64, end_time: f64) -> (i64, i64, i64) {
        let mut motion_sum = 0i64;
        let mut obj_sum = 0i64;
        let mut reg_sum = 0i64;
        for s in &self.samples {
            if s.timestamp >= start_time && s.timestamp <= end_time {
                motion_sum += s.motion_px as i64;
                obj_sum += s.active_objects as i64;
                reg_sum += s.regions as i64;
            }
        }
        (motion_sum, obj_sum, reg_sum)
    }
}

pub type SharedActivityLedger = Arc<RwLock<HashMap<String, CameraActivityRing>>>;

pub fn new_activity_ledger() -> SharedActivityLedger {
    Arc::new(RwLock::new(HashMap::new()))
}

fn unix_now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn random_suffix() -> String {
    let n: u32 = rand::random();
    format!("{n:06x}")
}

fn parse_duration_box(body: &[u8]) -> Option<f64> {
    if body.is_empty() {
        return None;
    }
    let ver = body[0];
    if ver == 1 && body.len() >= 32 {
        let ts = u32::from_be_bytes([body[20], body[21], body[22], body[23]]);
        let dur = u64::from_be_bytes([
            body[24], body[25], body[26], body[27], body[28], body[29], body[30], body[31],
        ]);
        if ts > 0 {
            return Some(dur as f64 / ts as f64);
        }
    } else if ver == 0 && body.len() >= 20 {
        let ts = u32::from_be_bytes([body[12], body[13], body[14], body[15]]);
        let dur = u32::from_be_bytes([body[16], body[17], body[18], body[19]]);
        if ts > 0 {
            return Some(dur as f64 / ts as f64);
        }
    }
    None
}

/// Reads the exact video track duration in seconds from an MP4 file's `vide` `mdhd` box
/// (falling back to `mvhd` if no `vide` track is present) in pure Rust without spawning `ffprobe`.
pub fn probe_mp4_duration_fast(path: &Path) -> Option<f64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    let mut pos = 0u64;
    let mut hdr = [0u8; 8];
    while pos + 8 <= file_len {
        file.seek(SeekFrom::Start(pos)).ok()?;
        file.read_exact(&mut hdr).ok()?;
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let btype = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let (box_size, hdr_len) = if size32 == 1 {
            let mut ext = [0u8; 8];
            file.read_exact(&mut ext).ok()?;
            (u64::from_be_bytes(ext), 16u64)
        } else if size32 == 0 {
            (file_len - pos, 8u64)
        } else {
            (size32, 8u64)
        };
        if box_size < hdr_len || pos + box_size > file_len {
            break;
        }
        if &btype == b"moov" {
            let mut moov = vec![0u8; (box_size - hdr_len).min(131_072) as usize];
            file.read_exact(&mut moov).ok()?;
            let mut fallback_mvhd = None;
            let mut mpos = 0usize;
            while mpos + 8 <= moov.len() {
                let msize = u32::from_be_bytes([
                    moov[mpos],
                    moov[mpos + 1],
                    moov[mpos + 2],
                    moov[mpos + 3],
                ]) as usize;
                let mtype = &moov[mpos + 4..mpos + 8];
                if msize < 8 || mpos + msize > moov.len() {
                    break;
                }
                let mbody = &moov[mpos + 8..mpos + msize];
                if mtype == b"mvhd" {
                    fallback_mvhd = parse_duration_box(mbody);
                } else if mtype == b"trak"
                    && let Some(vide_dur) = extract_vide_mdhd_duration(mbody)
                {
                    return Some(vide_dur);
                }
                mpos += msize;
            }
            return fallback_mvhd;
        }
        pos += box_size;
    }
    None
}

fn extract_vide_mdhd_duration(trak_body: &[u8]) -> Option<f64> {
    let mut tpos = 0usize;
    while tpos + 8 <= trak_body.len() {
        let tsize = u32::from_be_bytes([
            trak_body[tpos],
            trak_body[tpos + 1],
            trak_body[tpos + 2],
            trak_body[tpos + 3],
        ]) as usize;
        let ttype = &trak_body[tpos + 4..tpos + 8];
        if tsize < 8 || tpos + tsize > trak_body.len() {
            break;
        }
        if ttype == b"mdia" {
            let mdia = &trak_body[tpos + 8..tpos + tsize];
            let mut mdhd_dur = None;
            let mut is_vide = false;
            let mut dpos = 0usize;
            while dpos + 8 <= mdia.len() {
                let dsize = u32::from_be_bytes([
                    mdia[dpos],
                    mdia[dpos + 1],
                    mdia[dpos + 2],
                    mdia[dpos + 3],
                ]) as usize;
                let dtype = &mdia[dpos + 4..dpos + 8];
                if dsize < 8 || dpos + dsize > mdia.len() {
                    break;
                }
                let dbody = &mdia[dpos + 8..dpos + dsize];
                if dtype == b"mdhd" {
                    mdhd_dur = parse_duration_box(dbody);
                } else if dtype == b"hdlr" && dbody.len() >= 12 && &dbody[8..12] == b"vide" {
                    is_vide = true;
                }
                dpos += dsize;
            }
            if is_vide && mdhd_dur.is_some() {
                return mdhd_dur;
            }
        }
        tpos += tsize;
    }
    None
}

/// Parses the UTC start timestamp from a staged filename `{camera}_%Y%m%d%H%M%S.mp4`.
fn parse_staged_start_time(camera: &str, staged_path: &Path) -> Option<f64> {
    let stem = staged_path.file_stem()?.to_str()?;
    let date_part = stem.strip_prefix(&format!("{camera}_"))?;
    let ndt = NaiveDateTime::parse_from_str(date_part, "%Y%m%d%H%M%S").ok()?;
    Some(ndt.and_utc().timestamp() as f64)
}

/// Removes empty `<camera>`, `HH`, and `YYYY-MM-DD` parent directories after a segment file is deleted.
pub fn prune_empty_parent_dirs(file_path: &Path, stop_at: &Path) {
    let mut cur = file_path.parent();
    for _ in 0..3 {
        let Some(dir) = cur else {
            break;
        };
        if dir == stop_at || !dir.starts_with(stop_at) {
            break;
        }
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}

#[derive(Debug, Clone)]
pub struct CameraRecordPolicy {
    pub keep_continuous: bool,
    pub keep_motion: bool,
    pub retain_mode: String,
}

/// Manages direct segment recording and retention cleanup for all configured cameras.
pub struct RecordingEngine {
    config: Arc<RwLock<NinjaConfig>>,
    db: NinjaDatabase,
    media_dir: PathBuf,
    activity_ledger: SharedActivityLedger,
}

impl RecordingEngine {
    pub fn new(
        config: Arc<RwLock<NinjaConfig>>,
        db: NinjaDatabase,
        media_dir: PathBuf,
        activity_ledger: SharedActivityLedger,
    ) -> Self {
        Self {
            config,
            db,
            media_dir,
            activity_ledger,
        }
    }

    pub async fn start(self: Arc<Self>) {
        let cameras: Vec<(String, String, CameraRecordPolicy)> = {
            let cfg = self.config.read().await;
            cfg.cameras
                .iter()
                .filter(|(_, c)| c.enabled && c.record.enabled)
                .filter_map(|(name, c)| {
                    let input_url = cfg.camera_input_for_role(name, "record")?;
                    let policy = CameraRecordPolicy {
                        keep_continuous: c.record.continuous.days > 0.0,
                        keep_motion: c.record.motion.days > 0.0,
                        retain_mode: c.record.alerts.retain.mode.clone(),
                    };
                    Some((name.clone(), input_url, policy))
                })
                .collect()
        };

        for (camera_name, rtsp_url, policy) in cameras {
            let engine = self.clone();
            tokio::spawn(async move {
                engine
                    .run_camera_recorder_loop(camera_name, rtsp_url, policy)
                    .await;
            });
        }

        let cleanup_engine = self.clone();
        tokio::spawn(async move {
            cleanup_engine.run_retention_cleanup_loop().await;
        });

        let preview_engine = self.clone();
        tokio::spawn(async move {
            preview_engine.run_preview_generator_loop().await;
        });
    }

    async fn sweep_staging_dir(
        &self,
        camera: &str,
        staging_dir: &Path,
        policy: &CameraRecordPolicy,
    ) {
        let Ok(entries) = std::fs::read_dir(staging_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("mp4") {
                if probe_mp4_duration_fast(&path).is_some() {
                    let _ = self
                        .finalize_closed_segment(camera, &path, 10.0, policy)
                        .await;
                } else {
                    let _ = std::fs::remove_file(&path);
                }
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    async fn run_camera_recorder_loop(
        &self,
        camera: String,
        rtsp_url: String,
        policy: CameraRecordPolicy,
    ) {
        let staging_dir = self
            .media_dir
            .join("recordings")
            .join(".staging")
            .join(&camera);
        if let Err(err) = std::fs::create_dir_all(&staging_dir) {
            error!("Failed to create recording staging dir for {camera}: {err:#}");
            return;
        }

        let ffmpeg_bin = resolve_ffmpeg_bin();

        loop {
            self.sweep_staging_dir(&camera, &staging_dir, &policy).await;

            let out_pattern = staging_dir.join(format!("{camera}_%Y%m%d%H%M%S.mp4"));
            let mut cmd = Command::new(&ffmpeg_bin);
            cmd.env("TZ", "UTC")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-rtsp_transport",
                    "tcp",
                    "-timeout",
                    "10000000",
                    "-i",
                    &rtsp_url,
                    "-f",
                    "segment",
                    "-segment_time",
                    "10",
                    "-segment_format",
                    "mp4",
                    "-segment_format_options",
                    "movflags=+faststart",
                    "-reset_timestamps",
                    "1",
                    "-strftime",
                    "1",
                    "-segment_list",
                    "pipe:1",
                    "-segment_list_type",
                    "csv",
                    "-c:v",
                    "copy",
                    "-c:a",
                    "aac",
                ])
                .arg(out_pattern.to_string_lossy().as_ref())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);

            let Ok(mut child) = cmd.spawn() else {
                warn!(
                    "ffmpeg binary ({}) not available for camera {camera} recorder; retrying in 15s",
                    ffmpeg_bin.display()
                );
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                continue;
            };

            info!("Started direct segment recorder for camera {camera}");
            if let Some(stdout) = child.stdout.take() {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let parts: Vec<&str> = line.trim().split(',').collect();
                    if parts.len() >= 3 {
                        let seg_filename = parts[0];
                        let seg_dur = parts[2].parse::<f64>().unwrap_or(10.0)
                            - parts[1].parse::<f64>().unwrap_or(0.0);
                        let staged_path = staging_dir.join(seg_filename);
                        if let Err(err) = self
                            .finalize_closed_segment(
                                &camera,
                                &staged_path,
                                seg_dur.max(0.5),
                                &policy,
                            )
                            .await
                        {
                            warn!("Failed to finalize segment for {camera}: {err:#}");
                        }
                    }
                }
            }

            let status = child.wait().await;
            warn!("Recorder for {camera} exited ({status:?}); reconnecting in 5s");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    async fn finalize_closed_segment(
        &self,
        camera: &str,
        staged_path: &Path,
        csv_duration: f64,
        policy: &CameraRecordPolicy,
    ) -> Result<()> {
        let meta = std::fs::metadata(staged_path)
            .with_context(|| format!("missing staged segment {}", staged_path.display()))?;
        let size_bytes = meta.len();
        if size_bytes < 1024 {
            let _ = std::fs::remove_file(staged_path);
            return Ok(());
        }

        let duration = probe_mp4_duration_fast(staged_path)
            .unwrap_or(csv_duration)
            .clamp(0.5, 60.0);
        let start_time = parse_staged_start_time(camera, staged_path)
            .unwrap_or_else(|| unix_now_f64() - duration);
        let end_time = start_time + duration;

        let (motion, objects, regions) = {
            let ledger = self.activity_ledger.read().await;
            ledger
                .get(camera)
                .map(|ring| ring.aggregate_range(start_time, end_time))
                .unwrap_or((0, 0, 0))
        };

        let should_keep = if policy.keep_continuous || policy.retain_mode == "all" {
            true
        } else if policy.keep_motion || policy.retain_mode == "motion" {
            motion > 0 || objects > 0
        } else {
            objects > 0
        };

        if !should_keep {
            let _ = std::fs::remove_file(staged_path);
            return Ok(());
        }

        let dt = Utc
            .timestamp_opt(start_time as i64, 0)
            .single()
            .unwrap_or_else(Utc::now);
        let date_dir = dt.format("%Y-%m-%d").to_string();
        let hour_dir = dt.format("%H").to_string();
        let mut file_name = format!("{}.mp4", dt.format("%M.%S"));

        let target_dir = self
            .media_dir
            .join("recordings")
            .join(&date_dir)
            .join(&hour_dir)
            .join(camera);
        std::fs::create_dir_all(&target_dir)?;

        let mut final_disk_path = target_dir.join(&file_name);
        if final_disk_path.exists() {
            file_name = format!("{}-{}.mp4", dt.format("%M.%S"), &random_suffix()[..4]);
            final_disk_path = target_dir.join(&file_name);
        }

        std::fs::rename(staged_path, &final_disk_path).or_else(|_| {
            std::fs::copy(staged_path, &final_disk_path)?;
            std::fs::remove_file(staged_path)
        })?;

        let db_canonical_path =
            format!("/media/frigate/recordings/{date_dir}/{hour_dir}/{camera}/{file_name}");
        let seg_size_mb = ((size_bytes as f64) / (1024.0 * 1024.0) * 100.0).round() / 100.0;
        let id = format!("{:.1}-{}", start_time, random_suffix());

        self.db.insert_recording(&RecordingRow {
            id,
            camera: camera.to_string(),
            path: db_canonical_path,
            start_time,
            end_time,
            duration,
            motion,
            objects,
            dbfs: 0,
            segment_size: seg_size_mb,
            regions,
            motion_heatmap: None,
        })?;

        debug!(
            "Recorded segment for {camera}: {:.2}s ({:.2} MB, motion={}, objects={})",
            duration, seg_size_mb, motion, objects
        );
        Ok(())
    }

    async fn run_retention_cleanup_loop(&self) {
        let recordings_root = self.media_dir.join("recordings");
        loop {
            let (expire_interval, camera_retentions): (u64, Vec<(String, f64, f64, f64)>) = {
                let cfg = self.config.read().await;
                let interval = cfg.record.expire_interval.max(30);
                let cams = cfg
                    .cameras
                    .iter()
                    .map(|(name, c)| {
                        let continuous_days = c.record.continuous.days.max(0.1);
                        let alerts_days = c.record.alerts.retain.days.max(0.1);
                        let detections_days = c.record.detections.retain.days.max(0.1);
                        (name.clone(), continuous_days, alerts_days, detections_days)
                    })
                    .collect();
                (interval, cams)
            };

            let now = unix_now_f64();
            for (camera, continuous_days, alerts_days, detections_days) in camera_retentions {
                let continuous_cutoff = now - continuous_days * 86400.0;
                let alerts_cutoff = now - alerts_days * 86400.0;
                let detections_cutoff = now - detections_days * 86400.0;
                let event_recording_cutoff = alerts_cutoff.min(detections_cutoff);
                loop {
                    let Ok(expired_paths) = self.db.delete_expired_recordings(
                        &camera,
                        continuous_cutoff,
                        event_recording_cutoff,
                    ) else {
                        break;
                    };
                    if expired_paths.is_empty() {
                        break;
                    }
                    let batch_len = expired_paths.len();
                    let mut removed_files = 0usize;
                    for db_path in &expired_paths {
                        let disk_path = if let Some(rel) = db_path.strip_prefix("/media/frigate/") {
                            self.media_dir.join(rel)
                        } else {
                            PathBuf::from(db_path)
                        };
                        if std::fs::remove_file(&disk_path).is_ok() {
                            removed_files += 1;
                            prune_empty_parent_dirs(&disk_path, &recordings_root);
                        }
                    }
                    info!(
                        "Retention cleanup for {camera}: pruned {batch_len} DB rows and {removed_files} segment files"
                    );
                    if batch_len < 500 {
                        break;
                    }
                }

                if let Ok(clip_paths) = self.db.cleanup_expired_events_and_reviews(
                    &camera,
                    alerts_cutoff,
                    detections_cutoff,
                ) {
                    for cp in clip_paths {
                        let disk_path = if let Some(rel) = cp.strip_prefix("/media/frigate/") {
                            self.media_dir.join(rel)
                        } else {
                            PathBuf::from(cp)
                        };
                        let _ = std::fs::remove_file(disk_path);
                    }
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(expire_interval)).await;
        }
    }

    async fn run_preview_generator_loop(&self) {
        let ffmpeg_bin = resolve_ffmpeg_bin();
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;

        loop {
            let cameras: Vec<String> = {
                let cfg = self.config.read().await;
                cfg.cameras
                    .iter()
                    .filter(|(_, c)| c.enabled && c.record.enabled)
                    .map(|(n, _)| n.clone())
                    .collect()
            };

            let now = unix_now_f64();
            let current_hour_start = ((now as i64) / 3600) * 3600;
            let prev_hour_start = current_hour_start - 3600;

            for camera in cameras {
                for hour_start_i64 in [prev_hour_start, current_hour_start] {
                    let hour_start = hour_start_i64 as f64;
                    let hour_end = (hour_start_i64 + 3600) as f64;

                    let Ok(segs) = self.db.query_recordings(&camera, hour_start, hour_end) else {
                        continue;
                    };
                    if segs.len() < 2 {
                        continue;
                    }
                    let start_ts = segs
                        .first()
                        .map(|s| s.start_time.max(hour_start))
                        .unwrap_or(hour_start);
                    let end_ts = segs
                        .last()
                        .map(|s| s.end_time.min(hour_end))
                        .unwrap_or(hour_end.min(now));
                    let duration = (end_ts - start_ts).max(1.0);

                    let preview_dir = self.media_dir.join("clips").join("previews").join(&camera);
                    if std::fs::create_dir_all(&preview_dir).is_err() {
                        continue;
                    }
                    let file_name = format!("{hour_start_i64}-{}.mp4", hour_start_i64 + 3600);
                    let out_file = preview_dir.join(&file_name);
                    let list_file = std::env::temp_dir()
                        .join(format!("ninjanvr-prev-{camera}-{hour_start_i64}.txt"));

                    let mut list_content = String::new();
                    for seg in &segs {
                        let local_path = if let Some(rel) = seg.path.strip_prefix("/media/frigate/")
                        {
                            self.media_dir.join(rel)
                        } else {
                            PathBuf::from(&seg.path)
                        };
                        if local_path.exists() {
                            let escaped = local_path.to_string_lossy().replace('\'', "'\\''");
                            list_content.push_str(&format!("file '{escaped}'\n"));
                        }
                    }
                    if list_content.is_empty() || std::fs::write(&list_file, &list_content).is_err()
                    {
                        continue;
                    }

                    let status = Command::new(&ffmpeg_bin)
                        .args([
                            "-y",
                            "-hide_banner",
                            "-loglevel",
                            "error",
                            "-skip_frame",
                            "nokey",
                            "-f",
                            "concat",
                            "-safe",
                            "0",
                            "-i",
                        ])
                        .arg(&list_file)
                        .args([
                            "-vf",
                            "fps=1,scale=-2:180",
                            "-c:v",
                            "libx264",
                            "-preset",
                            "ultrafast",
                            "-crf",
                            "32",
                            "-an",
                            "-movflags",
                            "+faststart",
                        ])
                        .arg(&out_file)
                        .status()
                        .await;
                    let _ = std::fs::remove_file(&list_file);

                    if status.map(|s| s.success()).unwrap_or(false) && out_file.exists() {
                        let id = format!("{camera}_{hour_start_i64}");
                        let db_path = format!("/media/frigate/clips/previews/{camera}/{file_name}");
                        let _ = self
                            .db
                            .insert_preview(&id, &camera, &db_path, start_ts, end_ts, duration);
                    }
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        }
    }
}

/// Concatenates recording segments in `[start, end]` into a single MP4 export file.
pub async fn build_mp4_export(
    db: &NinjaDatabase,
    media_dir: &Path,
    export_id: &str,
    camera: &str,
    name: &str,
    start_time: f64,
    end_time: f64,
) -> Result<()> {
    let res =
        build_mp4_export_inner(db, media_dir, export_id, camera, name, start_time, end_time).await;
    if res.is_err() {
        let _ = db.delete_export(export_id);
    }
    res
}

async fn build_mp4_export_inner(
    db: &NinjaDatabase,
    media_dir: &Path,
    export_id: &str,
    camera: &str,
    name: &str,
    start_time: f64,
    end_time: f64,
) -> Result<()> {
    let segments = db.query_recordings(camera, start_time, end_time)?;
    if segments.is_empty() {
        anyhow::bail!("no recording segments found for {camera} in [{start_time}, {end_time}]");
    }

    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    std::fs::create_dir_all(&exports_dir)?;
    std::fs::create_dir_all(&clips_dir)?;

    let out_file = exports_dir.join(format!("{export_id}.mp4"));
    let thumb_file = clips_dir.join(format!("{export_id}.webp"));
    let list_file = std::env::temp_dir().join(format!("ninjanvr-export-{export_id}.txt"));

    let mut list_content = String::new();
    let mut existing_paths = Vec::new();
    for seg in &segments {
        let local_path = if let Some(rel) = seg.path.strip_prefix("/media/frigate/") {
            media_dir.join(rel)
        } else {
            PathBuf::from(&seg.path)
        };
        if local_path.exists() {
            let escaped = local_path.to_string_lossy().replace('\'', "'\\''");
            list_content.push_str(&format!("file '{escaped}'\n"));
            existing_paths.push(local_path);
        }
    }

    if existing_paths.is_empty() {
        anyhow::bail!("no segment files exist on disk for export {export_id}");
    }

    std::fs::write(&list_file, &list_content)?;

    let ffmpeg_bin = resolve_ffmpeg_bin();
    let status = Command::new(&ffmpeg_bin)
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
        ])
        .arg(&list_file)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(&out_file)
        .status()
        .await;

    let _ = std::fs::remove_file(&list_file);

    if status.map(|s| !s.success()).unwrap_or(true) {
        std::fs::copy(&existing_paths[0], &out_file)?;
    }

    // Generate a valid WebP thumbnail so the UI Exports grid always has an image
    let mut img = image::RgbImage::new(320, 180);
    for p in img.pixels_mut() {
        *p = image::Rgb([24, 30, 38]);
    }
    let _ = img.save_with_format(&thumb_file, image::ImageFormat::WebP);

    let video_rel = format!("/media/frigate/exports/{export_id}.mp4");
    let thumb_rel = format!("/media/frigate/clips/{export_id}.webp");
    db.insert_export(&ninjanvr_db::ExportRow {
        id: export_id.to_string(),
        camera: camera.to_string(),
        name: name.to_string(),
        date: unix_now_f64(),
        video_path: video_rel,
        thumb_path: thumb_rel,
        in_progress: false,
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_ring_aggregates_and_expires_old_samples() {
        let mut ring = CameraActivityRing::default();
        ring.record_sample(1000.0, 50, 1, 1);
        ring.record_sample(1200.0, 80, 2, 1);
        ring.record_sample(1350.0, 10, 0, 1);

        // 1000.0 is expired (>300s before 1350.0)
        let (m, o, _r) = ring.aggregate_range(900.0, 1400.0);
        assert_eq!(m, 90);
        assert_eq!(o, 2);
    }

    #[test]
    fn prune_empty_parent_dirs_removes_bottom_up_until_root() {
        let root = std::env::temp_dir().join(format!("ninjanvr-prune-{}", random_suffix()));
        let leaf_dir = root.join("2026-11-01").join("08").join("front_door");
        std::fs::create_dir_all(&leaf_dir).unwrap();
        let dummy_file = leaf_dir.join("00.10.mp4");
        std::fs::write(&dummy_file, b"test").unwrap();

        std::fs::remove_file(&dummy_file).unwrap();
        prune_empty_parent_dirs(&dummy_file, &root);

        assert!(!leaf_dir.exists());
        assert!(!root.join("2026-11-01").exists());
        assert!(root.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
