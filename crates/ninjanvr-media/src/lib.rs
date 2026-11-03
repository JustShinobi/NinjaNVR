use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{NaiveDateTime, TimeZone, Utc};
use ninjanvr_config::{AudioConfig, NinjaConfig};
use ninjanvr_db::{EventDto, NinjaDatabase, RecordingRow, ReviewItemDto, strip_media_root};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Probes whether `media_dir` (`recordings/`, `clips/`, `exports/`) can be created and written to.
/// Returns `Ok(())` if writable, or `Err(reason)` if the mount is missing, read-only, or permission-denied.
pub fn probe_storage_writable(media_dir: &Path) -> std::result::Result<(), String> {
    for sub in ["recordings", "clips", "exports"] {
        let dir = media_dir.join(sub);
        if let Err(err) = std::fs::create_dir_all(&dir) {
            return Err(format!("failed to initialize {}: {err}", dir.display()));
        }
    }
    let probe_file = media_dir.join("recordings").join(".ninjanvr_write_probe");
    if let Err(err) = std::fs::write(&probe_file, b"ok") {
        return Err(format!(
            "recordings mount {} is not writable: {err}",
            media_dir.display()
        ));
    }
    let _ = std::fs::remove_file(&probe_file);
    Ok(())
}

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
    pub dbfs: i64,
}

#[derive(Default)]
pub struct CameraActivityRing {
    samples: VecDeque<ActivitySample>,
    pub record_enabled: Option<bool>,
}

impl CameraActivityRing {
    pub fn set_record_enabled(&mut self, enabled: bool) {
        self.record_enabled = Some(enabled);
    }

    pub fn is_record_enabled(&self) -> bool {
        self.record_enabled.unwrap_or(true)
    }

    pub fn record_sample(
        &mut self,
        timestamp: f64,
        motion_px: u32,
        active_objects: u32,
        regions: u32,
    ) {
        self.record_sample_with_audio(timestamp, motion_px, active_objects, regions, 0);
    }

    pub fn record_sample_with_audio(
        &mut self,
        timestamp: f64,
        motion_px: u32,
        active_objects: u32,
        regions: u32,
        dbfs: i64,
    ) {
        self.samples.push_back(ActivitySample {
            timestamp,
            motion_px,
            active_objects,
            regions,
            dbfs,
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
        let (m, o, r, _) = self.aggregate_range_with_audio(start_time, end_time);
        (m, o, r)
    }

    pub fn aggregate_range_with_audio(
        &self,
        start_time: f64,
        end_time: f64,
    ) -> (i64, i64, i64, i64) {
        let mut motion_sum = 0i64;
        let mut obj_sum = 0i64;
        let mut reg_sum = 0i64;
        let mut peak_dbfs = 0i64;
        for s in &self.samples {
            if s.timestamp >= start_time && s.timestamp <= end_time {
                motion_sum += s.motion_px as i64;
                obj_sum += s.active_objects as i64;
                reg_sum += s.regions as i64;
                if s.dbfs != 0 && (peak_dbfs == 0 || s.dbfs > peak_dbfs) {
                    peak_dbfs = s.dbfs;
                }
            }
        }
        (motion_sum, obj_sum, reg_sum, peak_dbfs)
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
    let secs = if ver == 1 && body.len() >= 32 {
        let ts = u32::from_be_bytes([body[20], body[21], body[22], body[23]]);
        let dur = u64::from_be_bytes([
            body[24], body[25], body[26], body[27], body[28], body[29], body[30], body[31],
        ]);
        if ts > 0 {
            Some(dur as f64 / ts as f64)
        } else {
            None
        }
    } else if ver == 0 && body.len() >= 20 {
        let ts = u32::from_be_bytes([body[12], body[13], body[14], body[15]]);
        let dur = u32::from_be_bytes([body[16], body[17], body[18], body[19]]);
        if ts > 0 {
            Some(dur as f64 / ts as f64)
        } else {
            None
        }
    } else {
        None
    }?;
    if secs > 0.0 && secs <= 300.0 {
        Some(secs)
    } else {
        None
    }
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
            let mut saw_vide_trak = false;
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
                } else if mtype == b"trak" {
                    if let Some(vide_dur) = extract_vide_mdhd_duration(mbody) {
                        return Some(vide_dur);
                    }
                    if trak_has_vide_handler(mbody) {
                        saw_vide_trak = true;
                    }
                }
                mpos += msize;
            }
            return if saw_vide_trak { None } else { fallback_mvhd };
        }
        pos += box_size;
    }
    None
}

fn trak_has_vide_handler(trak_body: &[u8]) -> bool {
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
                if dtype == b"hdlr" && dbody.len() >= 12 && &dbody[8..12] == b"vide" {
                    return true;
                }
                dpos += dsize;
            }
        }
        tpos += tsize;
    }
    false
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

/// Computes the RMS level in dBFS (`-89` to `-1` for active audio, `0` for silence/empty)
/// from signed 16-bit PCM audio samples.
pub fn compute_pcm_rms_dbfs(samples: &[i16]) -> i64 {
    if samples.is_empty() {
        return 0;
    }
    let sum_sq: f64 = samples
        .iter()
        .map(|&s| {
            let norm = (s as f64) / 32768.0;
            norm * norm
        })
        .sum();
    let rms = (sum_sq / (samples.len() as f64)).sqrt();
    if rms <= 0.00005 {
        return 0;
    }
    let db = (20.0 * rms.log10()).round() as i64;
    if db <= -90 { 0 } else { db.clamp(-89, -1) }
}

/// Evaluates whether a measured `dbfs` level triggers an acoustic detection event
/// under the provided `AudioConfig`.
pub fn check_acoustic_event_trigger(dbfs: i64, cfg: &AudioConfig) -> Option<String> {
    if !cfg.enabled || dbfs == 0 {
        return None;
    }
    if dbfs >= (cfg.min_volume_dbfs as i64) {
        Some(
            cfg.labels
                .first()
                .cloned()
                .unwrap_or_else(|| "audio".to_string()),
        )
    } else {
        None
    }
}

fn decode_audio_dbfs_via_ffmpeg(path: &Path) -> i64 {
    let output = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar", "8000", "-f", "s16le", "pipe:1"])
        .output();
    let Ok(out) = output else {
        return 0;
    };
    if !out.status.success() || out.stdout.len() < 4 {
        return 0;
    }
    let pcm_samples: Vec<i16> = out
        .stdout
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(i16::from_le_bytes)
        .collect();
    compute_pcm_rms_dbfs(&pcm_samples)
}

/// Probes an MP4 segment file to extract the acoustic RMS level in dBFS
/// (`-89`..`-1` for active audio, `0` if silent or no `soun` track).
///
/// First inspects the ISO-BMFF `moov` -> `soun` track in pure Rust (`< 0.05ms`):
/// - If there is no `soun` track or `sample_count == 0`, returns `0` immediately without spawning FFmpeg.
/// - If `stsd` indicates a compressed codec (`mp4a` AAC, `alaw`, `ulaw`, `Opus`, etc.), decodes the
///   audio track to 8kHz mono `s16le` PCM via `ffmpeg` before computing RMS (avoiding raw compressed
///   bitstream byte reinterpretation, whose uniform entropy always averages ~`-5 dBFS`).
/// - If `stsd` is uncompressed PCM (or omitted in synthetic test containers), reads raw `s16le` samples
///   directly from the `mdat` chunk offsets.
pub fn probe_mp4_audio_dbfs_fast(path: &Path) -> i64 {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return 0;
    };
    let Ok(meta) = file.metadata() else {
        return 0;
    };
    let file_len = meta.len();
    let mut pos = 0u64;
    let mut hdr = [0u8; 8];
    while pos + 8 <= file_len {
        if file.seek(SeekFrom::Start(pos)).is_err() || file.read_exact(&mut hdr).is_err() {
            break;
        }
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let btype = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let (box_size, hdr_len) = if size32 == 1 {
            let mut ext = [0u8; 8];
            if file.read_exact(&mut ext).is_err() {
                break;
            }
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
            let mut moov = vec![0u8; (box_size - hdr_len).min(262_144) as usize];
            if file.read_exact(&mut moov).is_err() {
                break;
            }
            if let Some((offsets, total_audio_bytes, sample_count, is_compressed)) =
                extract_soun_sample_stats(&moov)
            {
                if sample_count == 0 || total_audio_bytes == 0 {
                    return 0;
                }
                if is_compressed {
                    return decode_audio_dbfs_via_ffmpeg(path);
                }
                let mut payload = Vec::with_capacity(8192);
                for off in offsets.into_iter().take(8) {
                    if payload.len() as u64 >= total_audio_bytes {
                        break;
                    }
                    if off < file_len && file.seek(SeekFrom::Start(off)).is_ok() {
                        let remaining_audio =
                            (total_audio_bytes.saturating_sub(payload.len() as u64)).min(2048);
                        let to_read = (file_len - off).min(remaining_audio) as usize;
                        if to_read == 0 {
                            break;
                        }
                        let mut chunk = vec![0u8; to_read];
                        if file.read_exact(&mut chunk).is_ok() {
                            payload.extend_from_slice(&chunk);
                        }
                    }
                }
                if payload.len() < 4 || payload.iter().all(|&b| b == 0) {
                    return 0;
                }
                let pcm_samples: Vec<i16> = payload
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .copied()
                    .map(i16::from_le_bytes)
                    .collect();
                return compute_pcm_rms_dbfs(&pcm_samples);
            }
            return 0;
        }
        pos += box_size;
    }
    0
}

fn extract_soun_sample_stats(moov: &[u8]) -> Option<(Vec<u64>, u64, u32, bool)> {
    let mut mpos = 0usize;
    while mpos + 8 <= moov.len() {
        let msize = u32::from_be_bytes([moov[mpos], moov[mpos + 1], moov[mpos + 2], moov[mpos + 3]])
            as usize;
        let mtype = &moov[mpos + 4..mpos + 8];
        if msize < 8 || mpos + msize > moov.len() {
            break;
        }
        if mtype == b"trak" {
            let trak = &moov[mpos + 8..mpos + msize];
            if let Some(stats) = parse_soun_trak(trak) {
                return Some(stats);
            }
        }
        mpos += msize;
    }
    None
}

fn parse_soun_trak(trak: &[u8]) -> Option<(Vec<u64>, u64, u32, bool)> {
    let mut tpos = 0usize;
    while tpos + 8 <= trak.len() {
        let tsize = u32::from_be_bytes([trak[tpos], trak[tpos + 1], trak[tpos + 2], trak[tpos + 3]])
            as usize;
        let ttype = &trak[tpos + 4..tpos + 8];
        if tsize < 8 || tpos + tsize > trak.len() {
            break;
        }
        if ttype == b"mdia" {
            let mdia = &trak[tpos + 8..tpos + tsize];
            let mut is_soun = false;
            let mut stbl_slice: Option<&[u8]> = None;
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
                if dtype == b"hdlr" && dbody.len() >= 12 && &dbody[8..12] == b"soun" {
                    is_soun = true;
                } else if dtype == b"minf" {
                    let mut ipos = 0usize;
                    while ipos + 8 <= dbody.len() {
                        let isize = u32::from_be_bytes([
                            dbody[ipos],
                            dbody[ipos + 1],
                            dbody[ipos + 2],
                            dbody[ipos + 3],
                        ]) as usize;
                        let itype = &dbody[ipos + 4..ipos + 8];
                        if isize < 8 || ipos + isize > dbody.len() {
                            break;
                        }
                        if itype == b"stbl" {
                            stbl_slice = Some(&dbody[ipos + 8..ipos + isize]);
                        }
                        ipos += isize;
                    }
                }
                dpos += dsize;
            }
            if is_soun && let Some(stbl) = stbl_slice {
                let mut offsets = Vec::new();
                let mut total_bytes = 0u64;
                let mut sample_count = 0u32;
                let mut is_compressed = false;
                let mut spos = 0usize;
                while spos + 8 <= stbl.len() {
                    let ssize = u32::from_be_bytes([
                        stbl[spos],
                        stbl[spos + 1],
                        stbl[spos + 2],
                        stbl[spos + 3],
                    ]) as usize;
                    let stype = &stbl[spos + 4..spos + 8];
                    if ssize < 8 || spos + ssize > stbl.len() {
                        break;
                    }
                    let sbody = &stbl[spos + 8..spos + ssize];
                    if stype == b"stsd" && sbody.len() >= 16 {
                        let fourcc = &sbody[12..16];
                        is_compressed =
                            !matches!(fourcc, b"sowt" | b"twos" | b"lpcm" | b"raw " | b"NONE");
                    } else if stype == b"stsz" && sbody.len() >= 12 {
                        let default_size =
                            u32::from_be_bytes([sbody[4], sbody[5], sbody[6], sbody[7]]);
                        let count = u32::from_be_bytes([sbody[8], sbody[9], sbody[10], sbody[11]]);
                        sample_count = count;
                        if default_size > 0 {
                            total_bytes = (default_size as u64) * (count as u64);
                        } else {
                            let mut bpos = 12usize;
                            while bpos + 4 <= sbody.len() {
                                let sz = u32::from_be_bytes([
                                    sbody[bpos],
                                    sbody[bpos + 1],
                                    sbody[bpos + 2],
                                    sbody[bpos + 3],
                                ]) as u64;
                                total_bytes += sz;
                                bpos += 4;
                            }
                        }
                    } else if stype == b"stco" && sbody.len() >= 8 {
                        let count =
                            u32::from_be_bytes([sbody[4], sbody[5], sbody[6], sbody[7]]) as usize;
                        let mut bpos = 8usize;
                        for _ in 0..count {
                            if bpos + 4 > sbody.len() {
                                break;
                            }
                            offsets.push(u32::from_be_bytes([
                                sbody[bpos],
                                sbody[bpos + 1],
                                sbody[bpos + 2],
                                sbody[bpos + 3],
                            ]) as u64);
                            bpos += 4;
                        }
                    } else if stype == b"co64" && sbody.len() >= 8 {
                        let count =
                            u32::from_be_bytes([sbody[4], sbody[5], sbody[6], sbody[7]]) as usize;
                        let mut bpos = 8usize;
                        for _ in 0..count {
                            if bpos + 8 > sbody.len() {
                                break;
                            }
                            offsets.push(u64::from_be_bytes([
                                sbody[bpos],
                                sbody[bpos + 1],
                                sbody[bpos + 2],
                                sbody[bpos + 3],
                                sbody[bpos + 4],
                                sbody[bpos + 5],
                                sbody[bpos + 6],
                                sbody[bpos + 7],
                            ]));
                            bpos += 8;
                        }
                    }
                    spos += ssize;
                }
                return Some((offsets, total_bytes, sample_count, is_compressed));
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

/// Parses a recording segment path (`.../YYYY-MM-DD/HH/<camera>/MM.SS.mp4`) into `(camera, start_time_utc)`.
pub fn parse_recording_segment_path(path: &Path) -> Option<(String, f64)> {
    if path.extension().and_then(|e| e.to_str()) != Some("mp4") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let mm_ss = stem.split('-').next()?;
    let (mm_str, ss_str) = mm_ss.split_once('.')?;
    let minute: u32 = mm_str.parse().ok()?;
    let second: u32 = ss_str.parse().ok()?;
    if minute >= 60 || second >= 60 {
        return None;
    }

    let cam_dir = path.parent()?;
    let camera = cam_dir.file_name()?.to_str()?;
    if camera.is_empty() || camera.starts_with('.') {
        return None;
    }

    let hour_dir = cam_dir.parent()?;
    let hour_str = hour_dir.file_name()?.to_str()?;
    let hour: u32 = hour_str.parse().ok()?;
    if hour >= 24 {
        return None;
    }

    let date_dir = hour_dir.parent()?;
    let date_str = date_dir.file_name()?.to_str()?;
    let date = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
    let ndt = date.and_hms_opt(hour, minute, second)?;
    Some((camera.to_string(), ndt.and_utc().timestamp() as f64))
}

/// Scans `media_dir/recordings/YYYY-MM-DD/HH/<camera>/MM.SS.mp4`, removes 0-byte orphan files,
/// and indexes valid MP4 recording segments into SQLite.
pub fn sync_recordings_from_disk(db: &NinjaDatabase, media_dir: &Path) -> Result<usize> {
    let rec_root = media_dir.join("recordings");
    if !rec_root.exists() {
        return Ok(0);
    }
    let mut synced = 0usize;
    let Ok(date_entries) = std::fs::read_dir(&rec_root) else {
        return Ok(0);
    };
    for date_entry in date_entries.flatten() {
        let date_path = date_entry.path();
        let Some(date_name) = date_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if date_name.starts_with('.') || !date_path.is_dir() {
            continue;
        }
        let Ok(hour_entries) = std::fs::read_dir(&date_path) else {
            continue;
        };
        for hour_entry in hour_entries.flatten() {
            let hour_path = hour_entry.path();
            let Some(hour_name) = hour_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if hour_name.starts_with('.') || !hour_path.is_dir() {
                continue;
            }
            let Ok(cam_entries) = std::fs::read_dir(&hour_path) else {
                continue;
            };
            for cam_entry in cam_entries.flatten() {
                let cam_path = cam_entry.path();
                if !cam_path.is_dir() {
                    continue;
                }
                let Ok(file_entries) = std::fs::read_dir(&cam_path) else {
                    continue;
                };
                for file_entry in file_entries.flatten() {
                    let file_path = file_entry.path();
                    if file_path.extension().and_then(|e| e.to_str()) != Some("mp4") {
                        continue;
                    }
                    let Ok(meta) = std::fs::metadata(&file_path) else {
                        continue;
                    };
                    if meta.len() == 0 {
                        let _ = std::fs::remove_file(&file_path);
                        continue;
                    }
                    let Some((camera, start_time)) = parse_recording_segment_path(&file_path)
                    else {
                        continue;
                    };
                    let Some(duration) = probe_mp4_duration_fast(&file_path) else {
                        continue;
                    };
                    let Some(file_name) = file_path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    let end_time = start_time + duration;
                    let dbfs = probe_mp4_audio_dbfs_fast(&file_path);
                    let seg_size_mb =
                        ((meta.len() as f64) / (1024.0 * 1024.0) * 100.0).round() / 100.0;
                    let db_canonical_path = format!(
                        "/media/ninjanvr/recordings/{date_name}/{hour_name}/{camera}/{file_name}"
                    );
                    let id = format!("{:.1}-{}", start_time, random_suffix());
                    db.insert_recording(&RecordingRow {
                        id,
                        camera,
                        path: db_canonical_path,
                        start_time,
                        end_time,
                        duration,
                        motion: 0,
                        objects: 0,
                        dbfs,
                        segment_size: seg_size_mb,
                        regions: 0,
                        motion_heatmap: None,
                    })?;
                    synced += 1;
                }
            }
        }
    }
    Ok(synced)
}

/// Builds the FFmpeg CLI arguments for segment recording, injecting `-itsoffset <sec>` and
/// stream mapping when `audio_offset_ms != 0` to compensate for RTSP audio/video lip-sync drift.
pub fn build_recorder_ffmpeg_args(
    rtsp_url: &str,
    out_pattern: &str,
    audio_offset_ms: i32,
) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-probesize".to_string(),
        "32768".to_string(),
        "-analyzeduration".to_string(),
        "500000".to_string(),
        "-threads".to_string(),
        "1".to_string(),
        "-rtsp_transport".to_string(),
        "tcp".to_string(),
        "-timeout".to_string(),
        "10000000".to_string(),
        "-i".to_string(),
        rtsp_url.to_string(),
    ];

    if audio_offset_ms != 0 {
        let offset_sec = (audio_offset_ms as f64) / 1000.0;
        args.extend([
            "-rtsp_transport".to_string(),
            "tcp".to_string(),
            "-timeout".to_string(),
            "10000000".to_string(),
            "-itsoffset".to_string(),
            format!("{offset_sec:.3}"),
            "-i".to_string(),
            rtsp_url.to_string(),
            "-map".to_string(),
            "0:v:0".to_string(),
            "-map".to_string(),
            "1:a:0?".to_string(),
        ]);
    }

    args.extend([
        "-f".to_string(),
        "segment".to_string(),
        "-segment_time".to_string(),
        "10".to_string(),
        "-segment_format".to_string(),
        "mp4".to_string(),
        "-reset_timestamps".to_string(),
        "1".to_string(),
        "-strftime".to_string(),
        "1".to_string(),
        "-segment_list".to_string(),
        "pipe:1".to_string(),
        "-segment_list_type".to_string(),
        "csv".to_string(),
        "-c:v".to_string(),
        "copy".to_string(),
        "-c:a".to_string(),
        "aac".to_string(),
        out_pattern.to_string(),
    ]);
    args
}

/// Builds the FFmpeg CLI arguments for Cold Compaction (`record.cold_compaction`),
/// supporting software (`libx264`, `libx265`) and hardware-accelerated (`vaapi`, `qsv`, `nvenc`)
/// encoders, presets, and CRF/QP targets.
pub fn build_cold_compaction_ffmpeg_args(
    src_path: &Path,
    dst_path: &Path,
    codec: &str,
    preset: &str,
    hwaccel: &str,
    crf: u8,
) -> Vec<String> {
    let is_hevc = matches!(codec.to_ascii_lowercase().as_str(), "hevc" | "h265");
    let safe_preset = match preset {
        "ultrafast" | "superfast" | "veryfast" | "faster" | "fast" | "medium" | "slow" => preset,
        _ => "veryfast",
    };
    let crf_str = crf.clamp(18, 36).to_string();
    let mut args = vec![
        "-y".to_string(),
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
    ];

    match hwaccel.to_ascii_lowercase().as_str() {
        "vaapi" => {
            args.extend([
                "-hwaccel".to_string(),
                "vaapi".to_string(),
                "-hwaccel_device".to_string(),
                "/dev/dri/renderD128".to_string(),
                "-hwaccel_output_format".to_string(),
                "vaapi".to_string(),
                "-i".to_string(),
                src_path.to_string_lossy().to_string(),
                "-c:v".to_string(),
                if is_hevc {
                    "hevc_vaapi".to_string()
                } else {
                    "h264_vaapi".to_string()
                },
                "-qp".to_string(),
                crf_str,
            ]);
        }
        "qsv" => {
            args.extend([
                "-hwaccel".to_string(),
                "qsv".to_string(),
                "-i".to_string(),
                src_path.to_string_lossy().to_string(),
                "-c:v".to_string(),
                if is_hevc {
                    "hevc_qsv".to_string()
                } else {
                    "h264_qsv".to_string()
                },
                "-preset".to_string(),
                safe_preset.to_string(),
                "-global_quality".to_string(),
                crf_str,
            ]);
        }
        "nvenc" => {
            let nv_preset = match safe_preset {
                "ultrafast" | "superfast" => "p1",
                "veryfast" | "faster" => "p2",
                "fast" => "p3",
                "medium" => "p4",
                "slow" => "p5",
                _ => "p2",
            };
            args.extend([
                "-hwaccel".to_string(),
                "cuda".to_string(),
                "-i".to_string(),
                src_path.to_string_lossy().to_string(),
                "-c:v".to_string(),
                if is_hevc {
                    "hevc_nvenc".to_string()
                } else {
                    "h264_nvenc".to_string()
                },
                "-preset".to_string(),
                nv_preset.to_string(),
                "-cq".to_string(),
                crf_str,
            ]);
        }
        _ => {
            args.extend([
                "-threads".to_string(),
                "1".to_string(),
                "-i".to_string(),
                src_path.to_string_lossy().to_string(),
                "-c:v".to_string(),
                if is_hevc {
                    "libx265".to_string()
                } else {
                    "libx264".to_string()
                },
                "-preset".to_string(),
                safe_preset.to_string(),
                "-crf".to_string(),
                crf_str,
                "-threads".to_string(),
                "1".to_string(),
            ]);
        }
    }

    if is_hevc {
        args.extend(["-tag:v".to_string(), "hvc1".to_string()]);
    }
    args.extend([
        "-c:a".to_string(),
        "copy".to_string(),
        "-movflags".to_string(),
        "+faststart".to_string(),
        dst_path.to_string_lossy().to_string(),
    ]);
    args
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
    pub continuous_substream: bool,
    pub retain_mode: String,
    pub audio_offset_ms: i32,
}

/// Manages direct segment recording and retention cleanup for all configured cameras.
pub struct RecordingEngine {
    config: Arc<RwLock<NinjaConfig>>,
    db: NinjaDatabase,
    media_dir: PathBuf,
    activity_ledger: SharedActivityLedger,
    storage_degraded: Arc<AtomicBool>,
    storage_error: Arc<std::sync::Mutex<Option<String>>>,
}

impl RecordingEngine {
    pub fn new(
        config: Arc<RwLock<NinjaConfig>>,
        db: NinjaDatabase,
        media_dir: PathBuf,
        activity_ledger: SharedActivityLedger,
    ) -> Self {
        let (degraded, err_msg) = match probe_storage_writable(&media_dir) {
            Ok(()) => (false, None),
            Err(e) => {
                warn!("Storage mount degraded at startup: {e}");
                (true, Some(e))
            }
        };
        Self {
            config,
            db,
            media_dir,
            activity_ledger,
            storage_degraded: Arc::new(AtomicBool::new(degraded)),
            storage_error: Arc::new(std::sync::Mutex::new(err_msg)),
        }
    }

    pub fn with_storage_state(
        config: Arc<RwLock<NinjaConfig>>,
        db: NinjaDatabase,
        media_dir: PathBuf,
        activity_ledger: SharedActivityLedger,
        storage_degraded: Arc<AtomicBool>,
        storage_error: Arc<std::sync::Mutex<Option<String>>>,
    ) -> Self {
        Self {
            config,
            db,
            media_dir,
            activity_ledger,
            storage_degraded,
            storage_error,
        }
    }

    pub async fn start(self: Arc<Self>) {
        let cameras: Vec<(String, String, CameraRecordPolicy)> = {
            let cfg = self.config.read().await;
            cfg.cameras
                .iter()
                .filter(|(_, c)| c.enabled && c.record.enabled)
                .filter_map(|(name, c)| {
                    let use_sub_default =
                        c.record.continuous_substream || cfg.record.continuous_substream;
                    let input_url = if use_sub_default && c.record.continuous.days == 0.0 {
                        cfg.camera_input_for_role(name, "detect")
                            .or_else(|| cfg.camera_input_for_role(name, "record"))?
                    } else {
                        cfg.camera_input_for_role(name, "record")?
                    };
                    let has_pre_post_buffer = c.record.alerts.pre_capture > 0
                        || c.record.alerts.post_capture > 0
                        || c.record.detections.pre_capture > 0
                        || c.record.detections.post_capture > 0;
                    let policy = CameraRecordPolicy {
                        keep_continuous: c.record.continuous.days > 0.0
                            || use_sub_default
                            || has_pre_post_buffer,
                        keep_motion: c.record.motion.days > 0.0,
                        continuous_substream: use_sub_default,
                        retain_mode: c.record.alerts.retain.mode.clone(),
                        audio_offset_ms: c.ffmpeg.audio_offset_ms.unwrap_or(0),
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

        let recovery_engine = self.clone();
        tokio::spawn(async move {
            recovery_engine.run_storage_recovery_probe_loop().await;
        });

        let dbfs_repair_engine = self.clone();
        tokio::spawn(async move {
            dbfs_repair_engine.run_legacy_dbfs_repair_once().await;
        });
    }

    async fn run_legacy_dbfs_repair_once(&self) {
        loop {
            let batch = self.db.list_recordings_by_dbfs(-5, 100).unwrap_or_default();
            if batch.is_empty() {
                break;
            }
            for (id, db_path) in batch {
                let rel =
                    strip_media_root(&db_path).unwrap_or_else(|| db_path.trim_start_matches('/'));
                let disk_path = self.media_dir.join(rel);
                let media_path = disk_path.clone();
                let real_dbfs = tokio::task::spawn_blocking(move || {
                    if media_path.exists() {
                        let d = probe_mp4_audio_dbfs_fast(&media_path);
                        if d == -5 { 0 } else { d }
                    } else {
                        0
                    }
                })
                .await
                .unwrap_or(0);
                let _ = self.db.update_recording_dbfs(&id, real_dbfs);
            }
            tokio::task::yield_now().await;
        }
    }

    async fn run_storage_recovery_probe_loop(&self) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            match probe_storage_writable(&self.media_dir) {
                Ok(()) => {
                    if self.storage_degraded.swap(false, Ordering::SeqCst) {
                        info!(
                            "Storage mount {} recovered; resuming normal recording operations",
                            self.media_dir.display()
                        );
                    }
                    if let Ok(mut guard) = self.storage_error.lock() {
                        *guard = None;
                    }
                }
                Err(err) => {
                    if !self.storage_degraded.swap(true, Ordering::SeqCst) {
                        warn!(
                            "Storage mount {} entered degraded state: {err}",
                            self.media_dir.display()
                        );
                    }
                    if let Ok(mut guard) = self.storage_error.lock() {
                        *guard = Some(err);
                    }
                }
            }
        }
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
        let ffmpeg_bin = resolve_ffmpeg_bin();

        loop {
            if let Err(err) = std::fs::create_dir_all(&staging_dir) {
                self.storage_degraded.store(true, Ordering::SeqCst);
                if let Ok(mut guard) = self.storage_error.lock() {
                    *guard = Some(format!(
                        "Failed to create recording staging dir {}: {err}",
                        staging_dir.display()
                    ));
                }
                warn!(
                    "Storage unavailable for {camera} staging dir ({}); retrying in 30s",
                    err
                );
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                continue;
            }
            self.sweep_staging_dir(&camera, &staging_dir, &policy).await;

            let out_pattern = staging_dir.join(format!("{camera}_%Y%m%d%H%M%S.mp4"));
            let ff_args = build_recorder_ffmpeg_args(
                &rtsp_url,
                out_pattern.to_string_lossy().as_ref(),
                policy.audio_offset_ms,
            );
            let mut cmd = Command::new(&ffmpeg_bin);
            cmd.env("TZ", "UTC")
                .args(&ff_args)
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

        let record_allowed = {
            let ledger = self.activity_ledger.read().await;
            ledger
                .get(camera)
                .map(|r| r.is_record_enabled())
                .unwrap_or(true)
        };
        if !record_allowed {
            let _ = std::fs::remove_file(staged_path);
            return Ok(());
        }

        let duration = probe_mp4_duration_fast(staged_path)
            .unwrap_or(csv_duration)
            .clamp(0.5, 60.0);
        let start_time = parse_staged_start_time(camera, staged_path)
            .unwrap_or_else(|| unix_now_f64() - duration);
        let end_time = start_time + duration;

        let (motion, objects, regions, ledger_dbfs) = {
            let ledger = self.activity_ledger.read().await;
            ledger
                .get(camera)
                .map(|ring| ring.aggregate_range_with_audio(start_time, end_time))
                .unwrap_or((0, 0, 0, 0))
        };
        let probed_dbfs = probe_mp4_audio_dbfs_fast(staged_path);
        let dbfs = if ledger_dbfs != 0 {
            ledger_dbfs
        } else {
            probed_dbfs
        };

        let should_keep = if policy.keep_continuous || policy.retain_mode == "all" {
            true
        } else if policy.keep_motion || policy.retain_mode == "motion" {
            motion > 0 || objects > 0 || (dbfs != 0 && dbfs > -45)
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
            format!("/media/ninjanvr/recordings/{date_dir}/{hour_dir}/{camera}/{file_name}");
        let seg_size_mb = ((size_bytes as f64) / (1024.0 * 1024.0) * 100.0).round() / 100.0;
        let id = format!("{:.1}-{}", start_time, random_suffix());

        self.db.insert_recording(&RecordingRow {
            id: id.clone(),
            camera: camera.to_string(),
            path: db_canonical_path,
            start_time,
            end_time,
            duration,
            motion,
            objects,
            dbfs,
            segment_size: seg_size_mb,
            regions,
            motion_heatmap: None,
        })?;

        // Emit dedicated acoustic event and ReviewSegment when audio exceeds threshold
        let audio_cfg = {
            let cfg = self.config.read().await;
            cfg.cameras
                .get(camera)
                .map(|c| {
                    if c.audio.enabled {
                        c.audio.clone()
                    } else {
                        cfg.audio.clone()
                    }
                })
                .unwrap_or_else(|| cfg.audio.clone())
        };
        if let Some(audio_label) = check_acoustic_event_trigger(dbfs, &audio_cfg) {
            let ev_id = format!("audio-{id}");
            let _ = self.db.upsert_event(&EventDto {
                id: ev_id.clone(),
                camera: camera.to_string(),
                label: "audio".to_string(),
                zones: json!([]),
                start_time,
                end_time: Some(end_time),
                has_clip: true,
                has_snapshot: false,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: Some(audio_label.clone()),
                top_score: Some(0.90),
                false_positive: Some(false),
                box_coords: None,
                data: json!({
                    "type": "audio",
                    "label": audio_label,
                    "dbfs": dbfs,
                    "score": 0.90,
                    "top_score": 0.90
                }),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.90),
                stationary: false,
            });
            let _ = self.db.insert_review_segment(&ReviewItemDto {
                id: format!("rev-{ev_id}"),
                camera: camera.to_string(),
                start_time,
                end_time: Some(end_time),
                severity: "detection".to_string(),
                thumb_path: format!("/api/{camera}/latest.jpg"),
                data: json!({
                    "detections": [ev_id],
                    "objects": ["audio"],
                    "zones": [],
                    "audio": [audio_label],
                    "dbfs": dbfs,
                    "thumb_time": start_time
                }),
                has_been_reviewed: false,
            });
        }

        debug!(
            "Recorded segment for {camera}: {:.2}s ({:.2} MB, motion={}, objects={}, dbfs={})",
            duration, seg_size_mb, motion, objects, dbfs
        );
        let tier = if policy.continuous_substream && objects == 0 && motion == 0 {
            "sub"
        } else {
            "main"
        };
        let _ = self.db.set_recording_stream_tier(&id, tier);
        Ok(())
    }

    async fn run_retention_cleanup_loop(&self) {
        let recordings_root = self.media_dir.join("recordings");
        let ffmpeg_bin = resolve_ffmpeg_bin();
        struct CameraRetentionEntry {
            camera: String,
            continuous_days: f64,
            motion_days: f64,
            alerts_days: f64,
            detections_days: f64,
            pre_capture_sec: f64,
            post_capture_sec: f64,
            quota_gb: Option<f64>,
            mode_tiering: bool,
            compact_enabled: bool,
            compact_after_hours: u32,
            compact_crf: u8,
            compact_idle_only: bool,
            compact_codec: String,
            compact_preset: String,
            compact_hwaccel: String,
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        loop {
            let (expire_interval, camera_retentions, global_quota_gb, global_min_free_gb): (
                u64,
                Vec<CameraRetentionEntry>,
                Option<f64>,
                Option<f64>,
            ) = {
                let cfg = self.config.read().await;
                let interval = cfg.record.expire_interval.max(30);
                let global_quota = cfg
                    .record
                    .max_size_gb
                    .or(cfg.record.alerts.retain.max_size_gb)
                    .or(cfg.record.detections.retain.max_size_gb);
                let global_min_free = cfg
                    .record
                    .min_free_gb
                    .or(cfg.record.alerts.retain.min_free_gb)
                    .or(cfg.record.detections.retain.min_free_gb);
                let cams = cfg
                    .cameras
                    .iter()
                    .map(|(name, c)| {
                        let pre_cap = c
                            .record
                            .alerts
                            .pre_capture
                            .max(c.record.detections.pre_capture)
                            .max(cfg.record.alerts.pre_capture)
                            as f64;
                        let post_cap = c
                            .record
                            .alerts
                            .post_capture
                            .max(c.record.detections.post_capture)
                            .max(cfg.record.alerts.post_capture)
                            as f64;
                        let min_buffer_days = ((pre_cap + post_cap + 60.0) / 86400.0).max(0.002);
                        let continuous_days = c.record.continuous.days.max(min_buffer_days);
                        let alerts_days = c.record.alerts.retain.days.max(0.1);
                        let detections_days = c.record.detections.retain.days.max(0.1);
                        let event_days = alerts_days.min(detections_days);
                        let motion_days = if c.record.motion.days > 0.0 {
                            c.record.motion.days.max(continuous_days)
                        } else if c.record.alerts.retain.mode != "active_objects"
                            || c.record.detections.retain.mode != "active_objects"
                        {
                            event_days.max(continuous_days)
                        } else {
                            continuous_days
                        };
                        let quota_gb = c
                            .record
                            .max_size_gb
                            .or(c.record.alerts.retain.max_size_gb)
                            .or(c.record.detections.retain.max_size_gb);
                        let mode_tiering = c.record.mode_tiering
                            || cfg.record.mode_tiering
                            || c.record.continuous_substream
                            || cfg.record.continuous_substream;
                        let compact_src = if c.record.cold_compaction.enabled {
                            &c.record.cold_compaction
                        } else {
                            &cfg.record.cold_compaction
                        };
                        let compact_enabled =
                            c.record.cold_compaction.enabled || cfg.record.cold_compaction.enabled;
                        let compact_after_hours = compact_src.after_hours.max(1);
                        let compact_crf = compact_src.crf.clamp(18, 36);
                        let compact_idle_only = compact_src.idle_only;
                        let compact_codec = compact_src.codec.clone();
                        let compact_preset = compact_src.preset.clone();
                        let compact_hwaccel = compact_src.hwaccel.clone();
                        CameraRetentionEntry {
                            camera: name.clone(),
                            continuous_days,
                            motion_days,
                            alerts_days,
                            detections_days,
                            pre_capture_sec: pre_cap,
                            post_capture_sec: post_cap,
                            quota_gb,
                            mode_tiering,
                            compact_enabled,
                            compact_after_hours,
                            compact_crf,
                            compact_idle_only,
                            compact_codec,
                            compact_preset,
                            compact_hwaccel,
                        }
                    })
                    .collect();
                (interval, cams, global_quota, global_min_free)
            };

            let now = unix_now_f64();
            for CameraRetentionEntry {
                camera,
                continuous_days,
                motion_days,
                alerts_days,
                detections_days,
                pre_capture_sec,
                post_capture_sec,
                quota_gb,
                mode_tiering,
                compact_enabled,
                compact_after_hours,
                compact_crf,
                compact_idle_only,
                compact_codec,
                compact_preset,
                compact_hwaccel,
            } in camera_retentions
            {
                if mode_tiering {
                    let idle_main_cutoff = now - (continuous_days.min(1.0) * 86400.0);
                    if let Ok(idle_paths) = self
                        .db
                        .delete_expired_idle_main_recordings(&camera, idle_main_cutoff)
                    {
                        for db_path in &idle_paths {
                            let disk_path = if let Some(rel) = strip_media_root(db_path) {
                                self.media_dir.join(rel)
                            } else {
                                PathBuf::from(db_path)
                            };
                            if std::fs::remove_file(&disk_path).is_ok() {
                                prune_empty_parent_dirs(&disk_path, &recordings_root);
                            }
                        }
                    }
                }
                let continuous_cutoff = now - continuous_days * 86400.0;
                let motion_cutoff = now - motion_days * 86400.0;
                let alerts_cutoff = now - alerts_days * 86400.0;
                let detections_cutoff = now - detections_days * 86400.0;
                let event_recording_cutoff = alerts_cutoff.min(detections_cutoff);
                loop {
                    let db_ref = self.db.clone();
                    let cam_ref = camera.clone();
                    let Ok(Ok(expired_paths)) = tokio::task::spawn_blocking(move || {
                        db_ref.delete_expired_recordings_with_window(
                            &cam_ref,
                            continuous_cutoff,
                            motion_cutoff,
                            event_recording_cutoff,
                            pre_capture_sec,
                            post_capture_sec,
                        )
                    })
                    .await
                    else {
                        break;
                    };
                    if expired_paths.is_empty() {
                        break;
                    }
                    let batch_len = expired_paths.len();
                    let mut removed_files = 0usize;
                    for db_path in &expired_paths {
                        let disk_path = if let Some(rel) = strip_media_root(db_path) {
                            self.media_dir.join(rel)
                        } else {
                            PathBuf::from(db_path)
                        };
                        if std::fs::remove_file(&disk_path).is_ok() {
                            removed_files += 1;
                            if disk_path.starts_with(&recordings_root) {
                                prune_empty_parent_dirs(&disk_path, &recordings_root);
                            } else if let Some(parent_root) = disk_path.ancestors().nth(4) {
                                prune_empty_parent_dirs(&disk_path, parent_root);
                            }
                        }
                    }
                    info!(
                        "Retention cleanup for {camera}: pruned {batch_len} DB rows and {removed_files} segment files"
                    );
                    tokio::task::yield_now().await;
                    if batch_len < 500 {
                        break;
                    }
                }

                if let Some(max_gb) = quota_gb
                    && max_gb > 0.0
                    && let Ok(quota_paths) = self
                        .db
                        .delete_recordings_over_quota_mb(&camera, max_gb * 1024.0)
                {
                    for db_path in &quota_paths {
                        let disk_path = if let Some(rel) = strip_media_root(db_path) {
                            self.media_dir.join(rel)
                        } else {
                            PathBuf::from(db_path)
                        };
                        if std::fs::remove_file(&disk_path).is_ok() {
                            prune_empty_parent_dirs(&disk_path, &recordings_root);
                        }
                    }
                }

                let db_ev = self.db.clone();
                let cam_ev = camera.clone();
                if let Ok(Ok(clip_paths)) = tokio::task::spawn_blocking(move || {
                    db_ev.cleanup_expired_events_and_reviews(
                        &cam_ev,
                        alerts_cutoff,
                        detections_cutoff,
                    )
                })
                .await
                {
                    for cp in clip_paths {
                        let disk_path = if let Some(rel) = strip_media_root(&cp) {
                            self.media_dir.join(rel)
                        } else {
                            PathBuf::from(cp)
                        };
                        let _ = std::fs::remove_file(disk_path);
                    }
                }

                if compact_enabled {
                    let compact_before = now - (compact_after_hours as f64) * 3600.0;
                    if let Ok(candidates) = self.db.list_cold_compaction_candidates(
                        &camera,
                        compact_before,
                        compact_idle_only,
                        pre_capture_sec,
                        post_capture_sec,
                        12,
                    ) {
                        for (seg_id, db_path) in candidates {
                            let disk_path = if let Some(rel) = strip_media_root(&db_path) {
                                self.media_dir.join(rel)
                            } else {
                                PathBuf::from(&db_path)
                            };
                            let Ok(orig_meta) = std::fs::metadata(&disk_path) else {
                                let _ = self.db.update_recording_compacted(&seg_id, 0.0);
                                continue;
                            };
                            let orig_len = orig_meta.len();
                            if orig_len < 65_536 {
                                let mb =
                                    ((orig_len as f64) / (1024.0 * 1024.0) * 100.0).round() / 100.0;
                                let _ = self.db.update_recording_compacted(&seg_id, mb);
                                continue;
                            }
                            let tmp_out = disk_path.with_extension("compact.mp4");
                            let primary_args = build_cold_compaction_ffmpeg_args(
                                &disk_path,
                                &tmp_out,
                                &compact_codec,
                                &compact_preset,
                                &compact_hwaccel,
                                compact_crf,
                            );
                            let mut status =
                                Command::new(&ffmpeg_bin).args(&primary_args).status().await;
                            if !status.as_ref().map(|s| s.success()).unwrap_or(false)
                                && compact_hwaccel != "none"
                            {
                                let fallback_args = build_cold_compaction_ffmpeg_args(
                                    &disk_path,
                                    &tmp_out,
                                    &compact_codec,
                                    &compact_preset,
                                    "none",
                                    compact_crf,
                                );
                                status = Command::new(&ffmpeg_bin)
                                    .args(&fallback_args)
                                    .status()
                                    .await;
                            }
                            let mut final_mb =
                                ((orig_len as f64) / (1024.0 * 1024.0) * 100.0).round() / 100.0;
                            if status.map(|s| s.success()).unwrap_or(false)
                                && let Ok(new_meta) = std::fs::metadata(&tmp_out)
                                && new_meta.len() > 4096
                                && new_meta.len() < orig_len
                                && probe_mp4_duration_fast(&tmp_out).is_some()
                            {
                                if std::fs::rename(&tmp_out, &disk_path).is_ok() {
                                    final_mb = ((new_meta.len() as f64) / (1024.0 * 1024.0)
                                        * 100.0)
                                        .round()
                                        / 100.0;
                                } else {
                                    let _ = std::fs::remove_file(&tmp_out);
                                }
                            } else {
                                let _ = std::fs::remove_file(&tmp_out);
                            }
                            let _ = self.db.update_recording_compacted(&seg_id, final_mb);
                        }
                    }
                }
            }

            // Global disk quota (`max_size_gb`) and minimum free disk headroom (`min_free_gb`) eviction pass
            let mut effective_global_max_mb = global_quota_gb.map(|g| g * 1024.0);
            if let Some(min_free_gb) = global_min_free_gb
                && min_free_gb > 0.0
                && let Ok(stat) = nix::sys::statvfs::statvfs(&recordings_root)
            {
                let bsize = stat.fragment_size() as f64;
                let free_mb = (stat.blocks_available() as f64 * bsize) / (1024.0 * 1024.0);
                let min_free_mb = min_free_gb * 1024.0;
                if free_mb < min_free_mb {
                    let deficit_mb = min_free_mb - free_mb;
                    if let Ok(usage_val) = self.db.query_recordings_storage_usage()
                        && let Some(obj) = usage_val.as_object()
                    {
                        let current_rec_mb: f64 =
                            obj.values().filter_map(|v| v["usage"].as_f64()).sum();
                        let allowed_mb = (current_rec_mb - deficit_mb).max(0.1);
                        effective_global_max_mb = Some(
                            effective_global_max_mb
                                .map(|m| m.min(allowed_mb))
                                .unwrap_or(allowed_mb),
                        );
                    }
                }
            }
            if let Some(max_mb) = effective_global_max_mb
                && max_mb > 0.0
                && let Ok(evicted_paths) = self.db.delete_global_recordings_over_quota_mb(max_mb)
            {
                for db_path in &evicted_paths {
                    let disk_path = if let Some(rel) = strip_media_root(db_path) {
                        self.media_dir.join(rel)
                    } else {
                        PathBuf::from(db_path)
                    };
                    if std::fs::remove_file(&disk_path).is_ok() {
                        prune_empty_parent_dirs(&disk_path, &recordings_root);
                    }
                }
            }

            // Hot-to-Cold tiered storage migration pass
            let tier_cfg = {
                let cfg = self.config.read().await;
                cfg.record.storage_tiers.clone()
            };
            if tier_cfg.enabled
                && let Some(ref cold_dir) = tier_cfg.cold_dir
            {
                let migrate_before = now - (tier_cfg.migrate_after_hours.max(1) as f64) * 3600.0;
                if let Err(err) = migrate_hot_segments_to_cold(
                    &self.db,
                    &self.media_dir,
                    cold_dir,
                    migrate_before,
                ) {
                    warn!("Hot-to-cold storage tier migration skipped: {err:#}");
                }
            }

            // Periodic SQLite query-planner optimization & WAL checkpointing
            let db_maint = self.db.clone();
            let _ = tokio::task::spawn_blocking(move || db_maint.run_maintenance()).await;

            tokio::time::sleep(std::time::Duration::from_secs(expire_interval)).await;
        }
    }

    async fn run_preview_generator_loop(&self) {
        let ffmpeg_bin = resolve_ffmpeg_bin();
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;

        loop {
            let cameras: Vec<(String, String)> = {
                let cfg = self.config.read().await;
                cfg.cameras
                    .iter()
                    .filter(|(_, c)| c.enabled && c.record.enabled)
                    .map(|(n, c)| (n.clone(), c.record.preview.quality.clone()))
                    .collect()
            };

            let now = unix_now_f64();
            let current_hour_start = ((now as i64) / 3600) * 3600;
            let prev_hour_start = current_hour_start - 3600;

            for (camera, preview_quality) in cameras {
                let (vf_filter, crf_val) = match preview_quality.as_str() {
                    "very_high" => ("fps=2,scale=-2:480", "23"),
                    "high" => ("fps=1,scale=-2:360", "26"),
                    "medium" => ("fps=1,scale=-2:240", "28"),
                    _ => ("fps=1,scale=-2:180", "32"),
                };
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
                    if let Ok(meta) = std::fs::metadata(&out_file)
                        && meta.len() > 1024
                    {
                        if hour_start_i64 == prev_hour_start {
                            // Previous hour preview already exists on disk; never regenerate
                            continue;
                        }
                        if let Ok(mtime) = meta.modified()
                            && mtime.elapsed().map(|d| d.as_secs() < 900).unwrap_or(false)
                        {
                            // Active hour preview was updated less than 15m ago
                            continue;
                        }
                    }
                    let list_file = std::env::temp_dir()
                        .join(format!("ninjanvr-prev-{camera}-{hour_start_i64}.txt"));

                    let mut list_content = String::new();
                    for seg in &segs {
                        let local_path = if let Some(rel) = strip_media_root(&seg.path) {
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
                            vf_filter,
                            "-c:v",
                            "libx264",
                            "-preset",
                            "ultrafast",
                            "-crf",
                            crf_val,
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
                        let db_path =
                            format!("/media/ninjanvr/clips/previews/{camera}/{file_name}");
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

/// Migrates recording segments older than `before_ts` from `hot_media_dir/recordings/`
/// to `cold_dir`, updating `recordings.path` in SQLite atomically and unlinking the hot source file.
pub fn migrate_hot_segments_to_cold(
    db: &NinjaDatabase,
    hot_media_dir: &Path,
    cold_dir: &Path,
    before_ts: f64,
) -> Result<usize> {
    if let Err(err) = std::fs::create_dir_all(cold_dir) {
        anyhow::bail!(
            "cold tier directory {} unreachable: {err}",
            cold_dir.display()
        );
    }
    let hot_rec_root = hot_media_dir.join("recordings");
    let mut migrated = 0usize;

    for _ in 0..5 {
        let candidates =
            db.list_recordings_for_tier_migration(before_ts, "/media/%/recordings/%", 200)?;
        if candidates.is_empty() {
            break;
        }
        let batch_len = candidates.len();
        for (id, db_path) in candidates {
            let Some(rel_under_rec) =
                strip_media_root(&db_path).and_then(|r| r.strip_prefix("recordings/"))
            else {
                continue;
            };
            let src_path = hot_rec_root.join(rel_under_rec);
            if !src_path.exists() {
                let _ = db.delete_recording_by_id(&id);
                continue;
            }
            let dst_path = cold_dir.join(rel_under_rec);
            let Some(dst_parent) = dst_path.parent() else {
                continue;
            };
            if let Err(err) = std::fs::create_dir_all(dst_parent) {
                warn!(
                    "Failed to create cold tier dir {}: {err}",
                    dst_parent.display()
                );
                return Ok(migrated);
            }
            let tmp_dst = dst_parent.join(format!(
                ".{}.tmp",
                dst_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("seg.mp4")
            ));
            if let Err(err) = std::fs::copy(&src_path, &tmp_dst) {
                let _ = std::fs::remove_file(&tmp_dst);
                warn!(
                    "Failed to copy segment {} to cold tier: {err}",
                    src_path.display()
                );
                return Ok(migrated);
            }
            if let Err(err) = std::fs::rename(&tmp_dst, &dst_path) {
                let _ = std::fs::remove_file(&tmp_dst);
                warn!(
                    "Failed to rename cold tier segment {}: {err}",
                    dst_path.display()
                );
                return Ok(migrated);
            }
            let new_db_path = dst_path.to_string_lossy().to_string();
            if db.update_recording_path(&id, &new_db_path)? {
                let _ = std::fs::remove_file(&src_path);
                prune_empty_parent_dirs(&src_path, &hot_rec_root);
                migrated += 1;
            }
        }
        if batch_len < 200 {
            break;
        }
    }
    Ok(migrated)
}

/// Automatically exports/copies a high-severity Alert clip and its best snapshot
/// into a secondary/offsite directory `target_dir/<YYYY-MM-DD>/<camera>_<ts>_<label>.mp4`.
#[allow(clippy::too_many_arguments)]
pub async fn backup_alert_clip(
    db: &NinjaDatabase,
    media_dir: &Path,
    target_dir: &Path,
    camera: &str,
    label: &str,
    start_time: f64,
    end_time: f64,
    thumb_rel: Option<&str>,
    include_snapshot: bool,
) -> Result<PathBuf> {
    let dt = Utc
        .timestamp_opt(start_time as i64, 0)
        .single()
        .unwrap_or_else(Utc::now);
    let date_dir = dt.format("%Y-%m-%d").to_string();
    let day_dir = target_dir.join(&date_dir);
    std::fs::create_dir_all(&day_dir)
        .with_context(|| format!("failed to create alert backup dir {}", day_dir.display()))?;

    let safe_label: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let stem = format!("{camera}_{}_{safe_label}", start_time as u64);
    let out_mp4 = day_dir.join(format!("{stem}.mp4"));

    let mut segments = db.query_recordings(camera, start_time, end_time)?;
    if segments.is_empty() {
        segments = db.query_recordings(camera, start_time - 15.0, end_time + 15.0)?;
    }
    let existing_paths: Vec<PathBuf> = segments
        .iter()
        .map(|s| {
            if let Some(rel) = strip_media_root(&s.path) {
                media_dir.join(rel)
            } else {
                PathBuf::from(&s.path)
            }
        })
        .filter(|p| p.exists())
        .collect();

    let Some(first_src) = existing_paths.first() else {
        anyhow::bail!("no recording segments found to back up for {camera}");
    };

    if existing_paths.len() == 1 {
        std::fs::copy(first_src, &out_mp4)
            .with_context(|| format!("failed to copy alert clip to {}", out_mp4.display()))?;
    } else {
        let list_path = day_dir.join(format!(".{stem}_concat.txt"));
        let mut list_content = String::new();
        for p in &existing_paths {
            let escaped = p.to_string_lossy().replace('\'', "'\\''");
            list_content.push_str(&format!("file '{escaped}'\n"));
        }
        let _ = std::fs::write(&list_path, &list_content);
        let dur = (end_time - start_time).max(10.0);
        let ff_args = build_mp4_export_ffmpeg_args(&list_path, 0.0, dur, true, &out_mp4);
        let ffmpeg_bin = resolve_ffmpeg_bin();
        let remux_ok = Command::new(&ffmpeg_bin)
            .args(&ff_args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|st| st.success())
            .unwrap_or(false);
        let _ = std::fs::remove_file(&list_path);
        if !remux_ok || !out_mp4.exists() {
            std::fs::copy(first_src, &out_mp4)
                .with_context(|| format!("failed to copy alert clip to {}", out_mp4.display()))?;
        }
    }

    if include_snapshot && let Some(t_rel) = thumb_rel {
        let thumb_disk = if let Some(rel) = strip_media_root(t_rel) {
            media_dir.join(rel)
        } else {
            PathBuf::from(t_rel)
        };
        if thumb_disk.exists() {
            let ext = thumb_disk
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("webp");
            let snap_dst = day_dir.join(format!("{stem}.{ext}"));
            if let Err(err) = std::fs::copy(&thumb_disk, &snap_dst) {
                warn!(
                    "Failed to copy alert snapshot {} to {}: {err}",
                    thumb_disk.display(),
                    snap_dst.display()
                );
            }
        }
    }

    Ok(out_mp4)
}

/// Builds the FFmpeg CLI arguments used to remux/concatenate segments into an exported MP4 clip.
/// When `include_audio` is `false`, `-an` is included to strip the audio stream for privacy compliance.
pub fn build_mp4_export_ffmpeg_args(
    list_file: &Path,
    trim_offset: f64,
    target_dur: f64,
    include_audio: bool,
    out_file: &Path,
) -> Vec<String> {
    build_mp4_export_ffmpeg_args_with_privacy(
        list_file,
        trim_offset,
        target_dur,
        include_audio,
        None,
        out_file,
    )
}

/// Builds the FFmpeg CLI arguments used to remux or transcode segments into an exported MP4 clip,
/// applying `build_privacy_drawbox_filter` when `privacy_mask` is configured.
pub fn build_mp4_export_ffmpeg_args_with_privacy(
    list_file: &Path,
    trim_offset: f64,
    target_dur: f64,
    include_audio: bool,
    privacy_mask: Option<&serde_json::Value>,
    out_file: &Path,
) -> Vec<String> {
    let mut args = vec![
        "-y".to_string(),
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
    ];
    if trim_offset > 0.05 {
        args.push("-ss".to_string());
        args.push(format!("{trim_offset:.2}"));
    }
    args.extend([
        "-f".to_string(),
        "concat".to_string(),
        "-safe".to_string(),
        "0".to_string(),
        "-i".to_string(),
        list_file.to_string_lossy().to_string(),
        "-t".to_string(),
        format!("{target_dur:.2}"),
        "-fflags".to_string(),
        "+genpts+igndts".to_string(),
        "-avoid_negative_ts".to_string(),
        "make_zero".to_string(),
    ]);
    if let Some(mask) = privacy_mask
        && let Some(vf) = build_privacy_drawbox_filter(mask)
    {
        args.extend([
            "-vf".to_string(),
            vf,
            "-c:a".to_string(),
            "copy".to_string(),
        ]);
    } else {
        args.extend([
            "-c:v".to_string(),
            "copy".to_string(),
            "-c:a".to_string(),
            "copy".to_string(),
        ]);
    }
    if !include_audio {
        args.push("-an".to_string());
    }
    args.extend([
        "-movflags".to_string(),
        "+faststart".to_string(),
        out_file.to_string_lossy().to_string(),
    ]);
    args
}

/// Converts a `privacy_mask` config value (normalized or pixel polygon strings) into an FFmpeg
/// `drawbox` video filter expression (`drawbox=x=iw*0.2:y=ih*0.2:w=iw*0.6:h=ih*0.6:color=black@1.0:t=fill`)
/// for redacting privacy regions during MP4 export transcoding.
pub fn build_privacy_drawbox_filter(privacy_mask: &serde_json::Value) -> Option<String> {
    let mut polys: Vec<String> = Vec::new();
    match privacy_mask {
        serde_json::Value::String(s) if !s.is_empty() => polys.push(s.clone()),
        serde_json::Value::Array(arr) => {
            for v in arr {
                if let Some(s) = v.as_str()
                    && !s.is_empty()
                {
                    polys.push(s.to_string());
                }
            }
        }
        _ => {}
    }
    let mut filters = Vec::new();
    for poly in polys {
        let nums: Vec<f32> = poly
            .split(',')
            .filter_map(|p| p.trim().parse::<f32>().ok())
            .collect();
        if nums.len() < 6 || !nums.len().is_multiple_of(2) {
            continue;
        }
        let mut x_min = f32::MAX;
        let mut y_min = f32::MAX;
        let mut x_max = f32::MIN;
        let mut y_max = f32::MIN;
        for &[px, py] in nums.as_chunks::<2>().0 {
            x_min = x_min.min(px);
            y_min = y_min.min(py);
            x_max = x_max.max(px);
            y_max = y_max.max(py);
        }
        if x_max > x_min && y_max > y_min {
            if x_max <= 1.0 && y_max <= 1.0 {
                filters.push(format!(
                    "drawbox=x=iw*{x_min:.3}:y=ih*{y_min:.3}:w=iw*{:.3}:h=ih*{:.3}:color=#0c0e12@1.0:t=fill",
                    x_max - x_min,
                    y_max - y_min
                ));
            } else {
                filters.push(format!(
                    "drawbox=x={}:y={}:w={}:h={}:color=#0c0e12@1.0:t=fill",
                    x_min as u32,
                    y_min as u32,
                    (x_max - x_min) as u32,
                    (y_max - y_min) as u32
                ));
            }
        }
    }
    if filters.is_empty() {
        None
    } else {
        Some(filters.join(","))
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
    build_mp4_export_with_options(
        db, media_dir, export_id, camera, name, start_time, end_time, true,
    )
    .await
}

/// Concatenates recording segments in `[start, end]` into a single MP4 export file,
/// optionally stripping the audio track when `include_audio` is `false`.
#[allow(clippy::too_many_arguments)]
pub async fn build_mp4_export_with_options(
    db: &NinjaDatabase,
    media_dir: &Path,
    export_id: &str,
    camera: &str,
    name: &str,
    start_time: f64,
    end_time: f64,
    include_audio: bool,
) -> Result<()> {
    build_mp4_export_with_privacy(
        db,
        media_dir,
        export_id,
        camera,
        name,
        start_time,
        end_time,
        include_audio,
        None,
    )
    .await
}

/// Concatenates recording segments in `[start, end]` into a single MP4 export file,
/// optionally stripping audio and redacting `privacy_mask` regions via FFmpeg `drawbox`.
#[allow(clippy::too_many_arguments)]
pub async fn build_mp4_export_with_privacy(
    db: &NinjaDatabase,
    media_dir: &Path,
    export_id: &str,
    camera: &str,
    name: &str,
    start_time: f64,
    end_time: f64,
    include_audio: bool,
    privacy_mask: Option<&serde_json::Value>,
) -> Result<()> {
    let res = build_mp4_export_inner(
        db,
        media_dir,
        export_id,
        camera,
        name,
        start_time,
        end_time,
        include_audio,
        privacy_mask,
    )
    .await;
    if res.is_err() {
        let _ = db.delete_export(export_id);
    }
    res
}

#[allow(clippy::too_many_arguments)]
async fn build_mp4_export_inner(
    db: &NinjaDatabase,
    media_dir: &Path,
    export_id: &str,
    camera: &str,
    name: &str,
    start_time: f64,
    end_time: f64,
    include_audio: bool,
    privacy_mask: Option<&serde_json::Value>,
) -> Result<()> {
    let mut segments = db.query_recordings(camera, start_time, end_time)?;
    if segments.is_empty() {
        segments = db.query_recordings(camera, start_time - 15.0, end_time + 15.0)?;
    }
    if segments.is_empty() {
        anyhow::bail!("no recording segments found for {camera} in [{start_time}, {end_time}]");
    }

    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    std::fs::create_dir_all(&exports_dir)?;
    std::fs::create_dir_all(&clips_dir)?;

    let out_file = exports_dir.join(format!("{export_id}.mp4"));
    let tmp_out_file = exports_dir.join(format!(".{export_id}.tmp.mp4"));
    let thumb_file = clips_dir.join(format!("{export_id}.webp"));
    let list_file = std::env::temp_dir().join(format!("ninjanvr-export-{export_id}.txt"));

    let mut list_content = String::new();
    let mut existing_paths = Vec::new();
    let mut first_seg_start = None;
    for seg in &segments {
        let local_path = if let Some(rel) = strip_media_root(&seg.path) {
            media_dir.join(rel)
        } else {
            PathBuf::from(&seg.path)
        };
        if local_path.exists() {
            if first_seg_start.is_none() {
                first_seg_start = Some(seg.start_time);
            }
            let escaped = local_path.to_string_lossy().replace('\'', "'\\''");
            list_content.push_str(&format!("file '{escaped}'\n"));
            existing_paths.push(local_path);
        }
    }

    if existing_paths.is_empty() {
        anyhow::bail!("no segment files exist on disk for export {export_id}");
    }

    std::fs::write(&list_file, &list_content)?;
    let _ = std::fs::remove_file(&tmp_out_file);

    let trim_offset = first_seg_start
        .map(|s0| (start_time - s0).max(0.0))
        .unwrap_or(0.0);
    let target_dur = (end_time - start_time).max(3.0);

    let ffmpeg_bin = resolve_ffmpeg_bin();
    let ffmpeg_args = build_mp4_export_ffmpeg_args_with_privacy(
        &list_file,
        trim_offset,
        target_dur,
        include_audio,
        privacy_mask,
        &tmp_out_file,
    );
    let mut cmd = Command::new(&ffmpeg_bin);
    cmd.kill_on_drop(true);
    let mut status = cmd.args(&ffmpeg_args).status().await;
    if (status.as_ref().map(|s| !s.success()).unwrap_or(true)
        || std::fs::metadata(&tmp_out_file)
            .map(|m| m.len() < 1024)
            .unwrap_or(true))
        && privacy_mask.is_some()
    {
        let fallback_args = build_mp4_export_ffmpeg_args(
            &list_file,
            trim_offset,
            target_dur,
            include_audio,
            &tmp_out_file,
        );
        status = Command::new(&ffmpeg_bin)
            .args(&fallback_args)
            .status()
            .await;
    }

    let _ = std::fs::remove_file(&list_file);

    if status.map(|s| !s.success()).unwrap_or(true)
        || std::fs::metadata(&tmp_out_file)
            .map(|m| m.len() < 1024)
            .unwrap_or(true)
    {
        std::fs::copy(&existing_paths[0], &tmp_out_file)?;
    }

    // Atomically publish completed MP4 only after +faststart rewrite has finished
    std::fs::rename(&tmp_out_file, &out_file)?;

    // Extract a real video frame thumbnail as WebP (with RgbImage fallback and privacy mask filter)
    let thumb_vf = if let Some(mask) = privacy_mask
        && let Some(db_f) = build_privacy_drawbox_filter(mask)
    {
        format!("{db_f},scale=320:-1")
    } else {
        "scale=320:-1".to_string()
    };
    let thumb_status = Command::new(&ffmpeg_bin)
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(&out_file)
        .args(["-frames:v", "1", "-vf", &thumb_vf])
        .arg(&thumb_file)
        .status()
        .await;

    if thumb_status.map(|s| !s.success()).unwrap_or(true) || !thumb_file.exists() {
        let mut img = image::RgbImage::new(320, 180);
        for p in img.pixels_mut() {
            *p = image::Rgb([24, 30, 38]);
        }
        let _ = img.save_with_format(&thumb_file, image::ImageFormat::WebP);
    }

    let video_rel = format!("/media/ninjanvr/exports/{export_id}.mp4");
    let thumb_rel = format!("/media/ninjanvr/clips/{export_id}.webp");
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
        ring.record_sample_with_audio(1000.0, 50, 1, 1, -40);
        ring.record_sample_with_audio(1200.0, 80, 2, 1, -22);
        ring.record_sample_with_audio(1350.0, 10, 0, 1, -35);

        // 1000.0 is expired (>300s before 1350.0)
        let (m, o, _r, dbfs) = ring.aggregate_range_with_audio(900.0, 1400.0);
        assert_eq!(m, 90);
        assert_eq!(o, 2);
        assert_eq!(dbfs, -22);
    }

    #[test]
    fn audio_rms_dbfs_and_mp4_probe_distinguish_silent_vs_active_segments() {
        assert_eq!(compute_pcm_rms_dbfs(&[]), 0);
        assert_eq!(compute_pcm_rms_dbfs(&[0i16; 1024]), 0);

        let loud_samples = vec![8192i16; 1024]; // ~ -12 dBFS
        let loud_db = compute_pcm_rms_dbfs(&loud_samples);
        assert!(
            (-15..0).contains(&loud_db),
            "expected ~ -12 dBFS, got {loud_db}"
        );

        fn make_box(btype: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let sz = (8 + body.len()) as u32;
            let mut out = Vec::with_capacity(sz as usize);
            out.extend_from_slice(&sz.to_be_bytes());
            out.extend_from_slice(btype);
            out.extend_from_slice(body);
            out
        }

        // Build synthetic MP4 with `soun` trak + `mdat`
        let build_synthetic_audio_mp4 = |pcm: &[i16]| -> Vec<u8> {
            let mut mdat_body = Vec::with_capacity(pcm.len() * 2);
            for &s in pcm {
                mdat_body.extend_from_slice(&s.to_le_bytes());
            }
            let ftyp = make_box(b"ftyp", &[0u8; 16]);
            let mdat_offset = (ftyp.len() + 8) as u32;
            let mdat = make_box(b"mdat", &mdat_body);

            let mut hdlr_body = vec![0u8; 12];
            hdlr_body[8..12].copy_from_slice(b"soun");
            let hdlr = make_box(b"hdlr", &hdlr_body);

            let mut stsz_body = Vec::new();
            stsz_body.extend_from_slice(&0u32.to_be_bytes()); // version/flags
            stsz_body.extend_from_slice(&(mdat_body.len() as u32).to_be_bytes()); // default sample size
            stsz_body.extend_from_slice(&1u32.to_be_bytes()); // sample count = 1
            let stsz = make_box(b"stsz", &stsz_body);

            let mut stco_body = Vec::new();
            stco_body.extend_from_slice(&0u32.to_be_bytes()); // version/flags
            stco_body.extend_from_slice(&1u32.to_be_bytes()); // count = 1
            stco_body.extend_from_slice(&mdat_offset.to_be_bytes());
            let stco = make_box(b"stco", &stco_body);

            let mut stbl_body = Vec::new();
            stbl_body.extend_from_slice(&stsz);
            stbl_body.extend_from_slice(&stco);
            let stbl = make_box(b"stbl", &stbl_body);
            let minf = make_box(b"minf", &stbl);

            let mut mdia_body = Vec::new();
            mdia_body.extend_from_slice(&hdlr);
            mdia_body.extend_from_slice(&minf);
            let mdia = make_box(b"mdia", &mdia_body);
            let trak = make_box(b"trak", &mdia);
            let moov = make_box(b"moov", &trak);

            let mut file_bytes = Vec::new();
            file_bytes.extend_from_slice(&ftyp);
            file_bytes.extend_from_slice(&mdat);
            file_bytes.extend_from_slice(&moov);
            file_bytes
        };

        let tmp_dir = std::env::temp_dir().join(format!("ninjanvr-audio-test-{}", random_suffix()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let silent_path = tmp_dir.join("silent.mp4");
        let active_path = tmp_dir.join("active.mp4");

        std::fs::write(&silent_path, build_synthetic_audio_mp4(&vec![0i16; 512])).unwrap();
        std::fs::write(
            &active_path,
            build_synthetic_audio_mp4(&vec![12000i16; 512]),
        )
        .unwrap();

        assert_eq!(probe_mp4_audio_dbfs_fast(&silent_path), 0);
        let probed_active = probe_mp4_audio_dbfs_fast(&active_path);
        assert!(
            (-15..0).contains(&probed_active),
            "expected active dBFS in [-15, -1], got {probed_active}"
        );

        // Verify that a `stsd` box with `mp4a` (compressed AAC) sets `is_compressed = true`
        // so compressed entropy bytes are never raw-cast as i16 PCM (which would yield -5 dBFS)
        let mut stsd_body = vec![0u8; 24];
        stsd_body[4..8].copy_from_slice(&1u32.to_be_bytes()); // entry count = 1
        stsd_body[8..12].copy_from_slice(&16u32.to_be_bytes()); // entry size = 16
        stsd_body[12..16].copy_from_slice(b"mp4a");
        let stsd = make_box(b"stsd", &stsd_body);
        let mut stsz_body = Vec::new();
        stsz_body.extend_from_slice(&0u32.to_be_bytes());
        stsz_body.extend_from_slice(&256u32.to_be_bytes());
        stsz_body.extend_from_slice(&1u32.to_be_bytes());
        let stsz = make_box(b"stsz", &stsz_body);
        let mut stco_body = Vec::new();
        stco_body.extend_from_slice(&0u32.to_be_bytes());
        stco_body.extend_from_slice(&1u32.to_be_bytes());
        stco_body.extend_from_slice(&24u32.to_be_bytes());
        let stco = make_box(b"stco", &stco_body);
        let mut stbl_body = Vec::new();
        stbl_body.extend_from_slice(&stsd);
        stbl_body.extend_from_slice(&stsz);
        stbl_body.extend_from_slice(&stco);
        let stbl = make_box(b"stbl", &stbl_body);
        let minf = make_box(b"minf", &stbl);
        let mut hdlr_body = vec![0u8; 12];
        hdlr_body[8..12].copy_from_slice(b"soun");
        let hdlr = make_box(b"hdlr", &hdlr_body);
        let mut mdia_body = Vec::new();
        mdia_body.extend_from_slice(&hdlr);
        mdia_body.extend_from_slice(&minf);
        let mdia = make_box(b"mdia", &mdia_body);
        let trak = make_box(b"trak", &mdia);
        let stats = extract_soun_sample_stats(&trak).expect("soun stats");
        assert!(stats.3, "expected mp4a stsd to be marked as compressed");

        let _ = std::fs::remove_dir_all(&tmp_dir);
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

    #[tokio::test]
    async fn build_mp4_export_atomically_publishes_mp4_and_clears_in_progress() {
        let root = std::env::temp_dir().join(format!("ninjanvr-export-test-{}", random_suffix()));
        let rec_dir = root.join("recordings/2026-11-01/12/front_door");
        std::fs::create_dir_all(&rec_dir).unwrap();

        let db_path = root.join("ninjanvr.db");
        let db = NinjaDatabase::open(&db_path).unwrap();

        let seg1_disk = rec_dir.join("00.00.mp4");
        let seg2_disk = rec_dir.join("00.10.mp4");
        // Write valid 2KB synthetic segment payloads
        let mut fake_mp4 = vec![0u8; 2048];
        fake_mp4[0..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p']);
        std::fs::write(&seg1_disk, &fake_mp4).unwrap();
        std::fs::write(&seg2_disk, &fake_mp4).unwrap();

        db.insert_recording(&RecordingRow {
            id: "seg1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-01/12/front_door/00.00.mp4".to_string(),
            start_time: 1000.0,
            end_time: 1010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: 0,
            segment_size: 0.002,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();
        db.insert_recording(&RecordingRow {
            id: "seg2".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-01/12/front_door/00.10.mp4".to_string(),
            start_time: 1010.0,
            end_time: 1020.0,
            duration: 10.0,
            motion: 2,
            objects: 0,
            dbfs: 0,
            segment_size: 0.002,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();

        let export_id = "front_door_1002_1018";
        build_mp4_export(
            &db,
            &root,
            export_id,
            "front_door",
            "Test Export Clip",
            1002.0,
            1018.0,
        )
        .await
        .unwrap();

        let final_mp4 = root.join("exports").join(format!("{export_id}.mp4"));
        let staging_mp4 = root.join("exports").join(format!(".{export_id}.tmp.mp4"));
        let thumb_webp = root.join("clips").join(format!("{export_id}.webp"));

        assert!(final_mp4.exists(), "final exported MP4 must exist");
        assert!(
            !staging_mp4.exists(),
            "staging .tmp.mp4 must be removed after atomic rename"
        );
        assert!(thumb_webp.exists(), "export thumbnail WebP must exist");

        let exports = db.query_exports().unwrap();
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0]["id"], export_id);
        assert_eq!(exports[0]["in_progress"], false);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn export_ffmpeg_args_include_or_strip_audio_track() {
        let list_path = Path::new("/tmp/list.txt");
        let out_path = Path::new("/tmp/out.mp4");

        let args_with_audio = build_mp4_export_ffmpeg_args(list_path, 2.5, 16.0, true, out_path);
        assert!(!args_with_audio.iter().any(|a| a == "-an"));

        let args_stripped_audio =
            build_mp4_export_ffmpeg_args(list_path, 2.5, 16.0, false, out_path);
        assert!(args_stripped_audio.iter().any(|a| a == "-an"));

        let rec_args_zero =
            build_recorder_ffmpeg_args("rtsp://127.0.0.1:8554/cam", "/tmp/cam_%Y.mp4", 0);
        assert!(!rec_args_zero.iter().any(|a| a == "-itsoffset"));

        let rec_args_offset =
            build_recorder_ffmpeg_args("rtsp://127.0.0.1:8554/cam", "/tmp/cam_%Y.mp4", -450);
        assert!(rec_args_offset.iter().any(|a| a == "-itsoffset"));
        assert!(rec_args_offset.iter().any(|a| a == "-0.450"));
        assert!(rec_args_offset.iter().any(|a| a == "1:a:0?"));
    }

    #[test]
    fn acoustic_event_trigger_evaluates_dbfs_threshold() {
        let mut cfg = AudioConfig::default();
        assert_eq!(check_acoustic_event_trigger(-18, &cfg), None);
        cfg.enabled = true;
        cfg.min_volume_dbfs = -24;
        assert_eq!(check_acoustic_event_trigger(0, &cfg), None);
        assert_eq!(check_acoustic_event_trigger(-40, &cfg), None);
        assert_eq!(
            check_acoustic_event_trigger(-18, &cfg),
            Some("audio".to_string())
        );
    }

    #[test]
    fn hot_to_cold_tiered_storage_migrates_segments_and_updates_sqlite_path() {
        let root = std::env::temp_dir().join(format!("ninjanvr-tier-test-{}", random_suffix()));
        let hot_media = root.join("hot_media");
        let cold_archive = root.join("cold_archive");
        let hot_seg_dir = hot_media.join("recordings/2026-11-01/08/front_door");
        std::fs::create_dir_all(&hot_seg_dir).unwrap();

        let db = NinjaDatabase::open(root.join("ninjanvr.db")).unwrap();
        let seg_file = hot_seg_dir.join("00.00.mp4");
        std::fs::write(&seg_file, b"mp4_segment_payload_bytes").unwrap();

        db.insert_recording(&RecordingRow {
            id: "seg_tier_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-01/08/front_door/00.00.mp4".to_string(),
            start_time: 1000.0,
            end_time: 1010.0,
            duration: 10.0,
            motion: 1,
            objects: 0,
            dbfs: 0,
            segment_size: 0.01,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();

        let migrated =
            migrate_hot_segments_to_cold(&db, &hot_media, &cold_archive, 2000.0).unwrap();
        assert_eq!(migrated, 1);
        assert!(
            !seg_file.exists(),
            "hot segment must be unlinked after migration"
        );

        let cold_seg_file = cold_archive.join("2026-11-01/08/front_door/00.00.mp4");
        assert!(cold_seg_file.exists(), "cold segment must exist in archive");

        let recs = db.query_recordings("front_door", 990.0, 1020.0).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].path, cold_seg_file.to_string_lossy());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn backup_alert_clip_copies_mp4_and_snapshot_to_secondary_target_dir() {
        let root = std::env::temp_dir().join(format!("ninjanvr-backup-test-{}", random_suffix()));
        let rec_dir = root.join("recordings/2026-11-01/12/front_door");
        let rev_dir = root.join("clips/review");
        let backup_dir = root.join("offsite_backup");
        std::fs::create_dir_all(&rec_dir).unwrap();
        std::fs::create_dir_all(&rev_dir).unwrap();

        let db = NinjaDatabase::open(root.join("ninjanvr.db")).unwrap();
        let seg_disk = rec_dir.join("00.00.mp4");
        std::fs::write(&seg_disk, b"alert_mp4_bytes").unwrap();
        let snap_disk = rev_dir.join("thumb-front_door-rev1.webp");
        std::fs::write(&snap_disk, b"alert_webp_bytes").unwrap();

        db.insert_recording(&RecordingRow {
            id: "seg_bak_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-01/12/front_door/00.00.mp4".to_string(),
            start_time: 1730500000.0,
            end_time: 1730500010.0,
            duration: 10.0,
            motion: 10,
            objects: 1,
            dbfs: 0,
            segment_size: 0.01,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

        let backed_up_mp4 = backup_alert_clip(
            &db,
            &root,
            &backup_dir,
            "front_door",
            "person",
            1730500000.0,
            1730500010.0,
            Some("/media/ninjanvr/clips/review/thumb-front_door-rev1.webp"),
            true,
        )
        .await
        .unwrap();

        assert!(backed_up_mp4.exists());
        assert!(backed_up_mp4.with_extension("webp").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mp4_duration_fast_and_extract_vide_mdhd_parse_v0_v1_mvhd_and_reject_invalid() {
        fn make_box(btype: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let sz = (8 + body.len()) as u32;
            let mut out = Vec::with_capacity(sz as usize);
            out.extend_from_slice(&sz.to_be_bytes());
            out.extend_from_slice(btype);
            out.extend_from_slice(body);
            out
        }

        fn make_mdhd_v0(timescale: u32, duration: u32) -> Vec<u8> {
            let mut body = vec![0u8; 24];
            body[0] = 0; // version 0
            body[12..16].copy_from_slice(&timescale.to_be_bytes());
            body[16..20].copy_from_slice(&duration.to_be_bytes());
            make_box(b"mdhd", &body)
        }

        fn make_mdhd_v1(timescale: u32, duration: u64) -> Vec<u8> {
            let mut body = vec![0u8; 36];
            body[0] = 1; // version 1 (64-bit)
            body[20..24].copy_from_slice(&timescale.to_be_bytes());
            body[24..32].copy_from_slice(&duration.to_be_bytes());
            make_box(b"mdhd", &body)
        }

        fn make_mvhd_v0(timescale: u32, duration: u32) -> Vec<u8> {
            let mut body = vec![0u8; 100];
            body[0] = 0;
            body[12..16].copy_from_slice(&timescale.to_be_bytes());
            body[16..20].copy_from_slice(&duration.to_be_bytes());
            make_box(b"mvhd", &body)
        }

        fn make_hdlr(handler: &[u8; 4]) -> Vec<u8> {
            let mut body = vec![0u8; 24];
            body[8..12].copy_from_slice(handler);
            make_box(b"hdlr", &body)
        }

        fn make_trak(mdhd: &[u8], handler: &[u8; 4]) -> Vec<u8> {
            let hdlr = make_hdlr(handler);
            let mut mdia_body = Vec::with_capacity(mdhd.len() + hdlr.len());
            mdia_body.extend_from_slice(mdhd);
            mdia_body.extend_from_slice(&hdlr);
            let mdia = make_box(b"mdia", &mdia_body);
            make_box(b"trak", &mdia)
        }

        fn wrap_mp4(moov_children: &[u8]) -> Vec<u8> {
            let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00isom");
            let moov = make_box(b"moov", moov_children);
            let mut out = Vec::with_capacity(ftyp.len() + moov.len());
            out.extend_from_slice(&ftyp);
            out.extend_from_slice(&moov);
            out
        }

        // 1. Direct extract_vide_mdhd_duration checks for v0 (10.0s) and v1 (15.0s)
        let trak_v0 = make_trak(&make_mdhd_v0(90_000, 900_000), b"vide");
        assert_eq!(extract_vide_mdhd_duration(&trak_v0[8..]), Some(10.0));

        let trak_v1 = make_trak(&make_mdhd_v1(48_000, 720_000), b"vide");
        assert_eq!(extract_vide_mdhd_duration(&trak_v1[8..]), Some(15.0));

        let trak_soun = make_trak(&make_mdhd_v0(48_000, 480_000), b"soun");
        assert_eq!(extract_vide_mdhd_duration(&trak_soun[8..]), None);

        // 2. File-level probe_mp4_duration_fast checks
        let tmp_dir = std::env::temp_dir().join(format!("ninjanvr-dur-probe-{}", random_suffix()));
        std::fs::create_dir_all(&tmp_dir).unwrap();

        // v0 mdhd (timescale=90000, duration=900000 -> 10.0s)
        let v0_path = tmp_dir.join("v0.mp4");
        let mut v0_moov = make_mvhd_v0(1000, 8000);
        v0_moov.extend_from_slice(&trak_v0);
        std::fs::write(&v0_path, wrap_mp4(&v0_moov)).unwrap();
        assert_eq!(probe_mp4_duration_fast(&v0_path), Some(10.0));

        // v1 mdhd (timescale=48000, duration=720000 -> 15.0s)
        let v1_path = tmp_dir.join("v1.mp4");
        let mut v1_moov = make_mvhd_v0(1000, 8000);
        v1_moov.extend_from_slice(&trak_v1);
        std::fs::write(&v1_path, wrap_mp4(&v1_moov)).unwrap();
        assert_eq!(probe_mp4_duration_fast(&v1_path), Some(15.0));

        // Fallback mvhd-only buffer when vide mdhd is absent
        let mvhd_only_path = tmp_dir.join("mvhd_only.mp4");
        let mut mvhd_moov = make_mvhd_v0(1000, 12_500);
        mvhd_moov.extend_from_slice(&trak_soun);
        std::fs::write(&mvhd_only_path, wrap_mp4(&mvhd_moov)).unwrap();
        assert_eq!(probe_mp4_duration_fast(&mvhd_only_path), Some(12.5));

        // Zero duration -> None
        let zero_path = tmp_dir.join("zero.mp4");
        let trak_zero = make_trak(&make_mdhd_v0(90_000, 0), b"vide");
        std::fs::write(&zero_path, wrap_mp4(&trak_zero)).unwrap();
        assert_eq!(probe_mp4_duration_fast(&zero_path), None);

        // Excessive (>300s) duration -> None
        let excessive_path = tmp_dir.join("excessive.mp4");
        let trak_excessive = make_trak(&make_mdhd_v0(1000, 350_000), b"vide");
        std::fs::write(&excessive_path, wrap_mp4(&trak_excessive)).unwrap();
        assert_eq!(probe_mp4_duration_fast(&excessive_path), None);

        // Truncated / non-MP4 -> None
        let corrupt_path = tmp_dir.join("corrupt.mp4");
        std::fs::write(&corrupt_path, b"not_an_mp4").unwrap();
        assert_eq!(probe_mp4_duration_fast(&corrupt_path), None);

        let truncated_path = tmp_dir.join("truncated.mp4");
        std::fs::write(
            &truncated_path,
            [0x00, 0x00, 0x01, 0x00, b'm', b'o', b'o', b'v'],
        )
        .unwrap();
        assert_eq!(probe_mp4_duration_fast(&truncated_path), None);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn privacy_drawbox_filter_and_export_ffmpeg_args_handle_normalized_and_empty_masks() {
        let mask_norm = json!(["0.10,0.20,0.50,0.20,0.50,0.80,0.10,0.80"]);
        let vf = build_privacy_drawbox_filter(&mask_norm).expect("expected drawbox filter");
        assert_eq!(
            vf,
            "drawbox=x=iw*0.100:y=ih*0.200:w=iw*0.400:h=ih*0.600:color=#0c0e12@1.0:t=fill"
        );

        let mask_px = json!("100,200,500,200,500,800,100,800");
        let vf_px = build_privacy_drawbox_filter(&mask_px).expect("expected pixel drawbox filter");
        assert_eq!(
            vf_px,
            "drawbox=x=100:y=200:w=400:h=600:color=#0c0e12@1.0:t=fill"
        );

        assert_eq!(build_privacy_drawbox_filter(&json!(null)), None);
        assert_eq!(build_privacy_drawbox_filter(&json!([])), None);
        assert_eq!(build_privacy_drawbox_filter(&json!("")), None);
        assert_eq!(build_privacy_drawbox_filter(&json!(["0.1,0.2"])), None);

        let list_path = Path::new("/tmp/list.txt");
        let out_path = Path::new("/tmp/out.mp4");

        let args_masked = build_mp4_export_ffmpeg_args_with_privacy(
            list_path,
            1.25,
            10.0,
            true,
            Some(&mask_norm),
            out_path,
        );
        assert!(args_masked.iter().any(|a| a == "-vf"));
        assert!(args_masked.iter().any(|a| a.starts_with("drawbox=")));
        assert!(!args_masked.iter().any(|a| a == "-c:v"));

        let args_empty = build_mp4_export_ffmpeg_args_with_privacy(
            list_path,
            0.0,
            10.0,
            true,
            Some(&json!([])),
            out_path,
        );
        assert!(!args_empty.iter().any(|a| a == "-vf"));
        assert!(args_empty.windows(2).any(|w| w == ["-c:v", "copy"]));

        let args_none =
            build_mp4_export_ffmpeg_args_with_privacy(list_path, 0.0, 10.0, false, None, out_path);
        assert!(!args_none.iter().any(|a| a == "-vf"));
        assert!(args_none.windows(2).any(|w| w == ["-c:v", "copy"]));
        assert!(args_none.iter().any(|a| a == "-an"));
    }

    #[test]
    fn sync_recordings_from_disk_and_probe_storage_writable_index_valid_and_prune_orphans() {
        fn make_box(btype: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let sz = (8 + body.len()) as u32;
            let mut out = Vec::with_capacity(sz as usize);
            out.extend_from_slice(&sz.to_be_bytes());
            out.extend_from_slice(btype);
            out.extend_from_slice(body);
            out
        }

        let root = std::env::temp_dir().join(format!("ninjanvr-sync-test-{}", random_suffix()));
        std::fs::create_dir_all(&root).unwrap();

        // 1. probe_storage_writable on writable directory
        assert!(probe_storage_writable(&root).is_ok());
        assert!(root.join("recordings").is_dir());
        assert!(root.join("clips").is_dir());
        assert!(root.join("exports").is_dir());

        // 2. probe_storage_writable on non-writable path (regular file blocking directory creation)
        let blocked_root = root.join("blocked_mount");
        std::fs::write(&blocked_root, b"not_a_dir").unwrap();
        assert!(probe_storage_writable(&blocked_root).is_err());

        // 3. parse_recording_segment_path & sync_recordings_from_disk
        let cam_dir = root.join("recordings/2026-11-02/14/front_door");
        std::fs::create_dir_all(&cam_dir).unwrap();

        let valid_seg_path = cam_dir.join("05.10.mp4");
        let orphan_zero_path = cam_dir.join("05.20.mp4");

        let (parsed_cam, parsed_ts) =
            parse_recording_segment_path(&valid_seg_path).expect("valid segment path");
        assert_eq!(parsed_cam, "front_door");
        let expected_ts = chrono::NaiveDate::from_ymd_opt(2026, 11, 2)
            .unwrap()
            .and_hms_opt(14, 5, 10)
            .unwrap()
            .and_utc()
            .timestamp() as f64;
        assert_eq!(parsed_ts, expected_ts);
        assert_eq!(
            parse_recording_segment_path(Path::new("/tmp/recordings/2026-11-02/99/cam/05.10.mp4")),
            None
        );

        // Write valid synthetic 10.0s MP4 to 05.10.mp4 and 0-byte orphan to 05.20.mp4
        let mut mdhd_body = vec![0u8; 24];
        mdhd_body[12..16].copy_from_slice(&90_000u32.to_be_bytes());
        mdhd_body[16..20].copy_from_slice(&900_000u32.to_be_bytes());
        let mdhd = make_box(b"mdhd", &mdhd_body);
        let mut hdlr_body = vec![0u8; 24];
        hdlr_body[8..12].copy_from_slice(b"vide");
        let hdlr = make_box(b"hdlr", &hdlr_body);
        let mut mdia_body = Vec::new();
        mdia_body.extend_from_slice(&mdhd);
        mdia_body.extend_from_slice(&hdlr);
        let mdia = make_box(b"mdia", &mdia_body);
        let trak = make_box(b"trak", &mdia);
        let moov = make_box(b"moov", &trak);
        let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00isom");
        let mut valid_mp4 = Vec::new();
        valid_mp4.extend_from_slice(&ftyp);
        valid_mp4.extend_from_slice(&moov);

        std::fs::write(&valid_seg_path, &valid_mp4).unwrap();
        std::fs::write(&orphan_zero_path, b"").unwrap();

        let db = NinjaDatabase::open(root.join("ninjanvr.db")).unwrap();
        let synced = sync_recordings_from_disk(&db, &root).unwrap();
        assert_eq!(synced, 1);
        assert!(
            !orphan_zero_path.exists(),
            "0-byte orphan MP4 must be removed during sync"
        );

        let rows = db
            .query_recordings("front_door", expected_ts - 5.0, expected_ts + 25.0)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].camera, "front_door");
        assert_eq!(rows[0].start_time, expected_ts);
        assert_eq!(rows[0].duration, 10.0);
        assert_eq!(
            rows[0].path,
            "/media/ninjanvr/recordings/2026-11-02/14/front_door/05.10.mp4"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn recording_engine_sweep_and_finalize_closed_segment_index_and_prune_staged_files() {
        fn make_box(btype: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let sz = (8 + body.len()) as u32;
            let mut out = Vec::with_capacity(sz as usize);
            out.extend_from_slice(&sz.to_be_bytes());
            out.extend_from_slice(btype);
            out.extend_from_slice(body);
            out
        }

        let root = std::env::temp_dir().join(format!("ninjanvr-finalize-test-{}", random_suffix()));
        let staging_dir = root.join("recordings/.staging/front_door");
        std::fs::create_dir_all(&staging_dir).unwrap();

        let db = NinjaDatabase::open(root.join("ninjanvr.db")).unwrap();
        let cfg = Arc::new(RwLock::new(
            NinjaConfig::from_yaml_str("cameras: {}\n", root.join("config.yml")).unwrap(),
        ));
        let ledger = new_activity_ledger();
        let engine = RecordingEngine::new(cfg, db.clone(), root.clone(), ledger);

        // Build >1KB valid synthetic MP4 with v0 mdhd (10.0s)
        let mut mdhd_body = vec![0u8; 24];
        mdhd_body[12..16].copy_from_slice(&90_000u32.to_be_bytes());
        mdhd_body[16..20].copy_from_slice(&900_000u32.to_be_bytes());
        let mdhd = make_box(b"mdhd", &mdhd_body);
        let mut hdlr_body = vec![0u8; 24];
        hdlr_body[8..12].copy_from_slice(b"vide");
        let hdlr = make_box(b"hdlr", &hdlr_body);
        let mut mdia_body = Vec::new();
        mdia_body.extend_from_slice(&mdhd);
        mdia_body.extend_from_slice(&hdlr);
        let mdia = make_box(b"mdia", &mdia_body);
        let trak = make_box(b"trak", &mdia);
        let moov = make_box(b"moov", &trak);
        let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00isom");
        let mdat = make_box(b"mdat", &vec![0u8; 1500]);
        let mut valid_mp4 = Vec::new();
        valid_mp4.extend_from_slice(&ftyp);
        valid_mp4.extend_from_slice(&mdat);
        valid_mp4.extend_from_slice(&moov);

        let staged_valid = staging_dir.join("front_door_20261102140510.mp4");
        let staged_corrupt = staging_dir.join("front_door_20261102140520.mp4");
        std::fs::write(&staged_valid, &valid_mp4).unwrap();
        std::fs::write(&staged_corrupt, b"").unwrap();

        let policy = CameraRecordPolicy {
            keep_continuous: true,
            keep_motion: true,
            continuous_substream: false,
            retain_mode: "all".to_string(),
            audio_offset_ms: 0,
        };

        engine
            .sweep_staging_dir("front_door", &staging_dir, &policy)
            .await;

        assert!(!staged_valid.exists());
        assert!(!staged_corrupt.exists());

        let expected_ts = chrono::NaiveDate::from_ymd_opt(2026, 11, 2)
            .unwrap()
            .and_hms_opt(14, 5, 10)
            .unwrap()
            .and_utc()
            .timestamp() as f64;
        let recs = db
            .query_recordings("front_door", expected_ts - 5.0, expected_ts + 25.0)
            .unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].duration, 10.0);

        let _ = std::fs::remove_dir_all(&root);
    }
}
