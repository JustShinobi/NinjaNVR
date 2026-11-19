#![forbid(unsafe_code)]
//! MP4 segment recording supervision, YUV420p/audio substream ingestion, tiered storage
//! retention cleanup, cold HEVC compaction, and privacy-masked clip export.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{NaiveDateTime, TimeZone, Utc};
use ninjanvr_config::{
    AudioConfig, NinjaConfig, O_NOFOLLOW_FLAG, create_new_file_nofollow, open_regular_file_nofollow,
};
use ninjanvr_db::{EventDto, NinjaDatabase, RecordingRow, ReviewItemDto, strip_media_root};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

fn copy_file_via_nofollow(src: &Path, dst_new: &Path) -> std::io::Result<u64> {
    let (mut src_file, _) = open_regular_file_nofollow(src)?;
    let mut dst_file = create_new_file_nofollow(dst_new)?;
    let bytes = std::io::copy(&mut src_file, &mut dst_file)?;
    dst_file.sync_all()?;
    Ok(bytes)
}

fn is_allowed_media_artifact_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    ext.eq_ignore_ascii_case("jpg")
        || ext.eq_ignore_ascii_case("jpeg")
        || ext.eq_ignore_ascii_case("webp")
        || ext.eq_ignore_ascii_case("mp4")
        || ext.eq_ignore_ascii_case("mask_hash")
}

/// Probes whether `media_dir` (`recordings/`, `clips/`, `exports/`) can be created and written to.
/// Returns `Ok(())` if writable, or `Err(reason)` if the mount is missing, read-only, or permission-denied.
pub fn probe_storage_writable(media_dir: &Path) -> std::result::Result<(), String> {
    for sub in ["recordings", "clips", "exports"] {
        let dir = media_dir.join(sub);
        if let Err(err) = std::fs::create_dir_all(&dir) {
            return Err(format!("failed to initialize {}: {err}", dir.display()));
        }
    }
    let nonce: u64 = rand::random();
    let probe_file = media_dir
        .join("recordings")
        .join(format!(".ninjanvr_write_probe.{nonce:016x}"));
    let _probe_guard = TempPathGuard(probe_file.clone());
    let write_res = (|| -> std::io::Result<()> {
        let mut f = create_new_file_nofollow(&probe_file)?;
        std::io::Write::write_all(&mut f, b"ok")?;
        Ok(())
    })();
    if let Err(err) = write_res {
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

pub use ninjanvr_vision::YuvFrame;

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

pub const MAX_ACTIVITY_SAMPLES: usize = 4_096;
pub const MAX_RETENTION_DAYS: f64 = 36_500.0;
pub const MAX_STORAGE_QUOTA_GB: f64 = 1_000_000.0;

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

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
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
        if !timestamp.is_finite() || timestamp < 0.0 {
            return;
        }
        let cutoff = timestamp - 300.0;
        while let Some(front) = self.samples.front() {
            if !front.timestamp.is_finite() || front.timestamp < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        self.samples
            .retain(|s| s.timestamp.is_finite() && s.timestamp >= cutoff);
        while self.samples.len() >= MAX_ACTIVITY_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(ActivitySample {
            timestamp,
            motion_px,
            active_objects,
            regions,
            dbfs,
        });
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
        if !start_time.is_finite() || !end_time.is_finite() || end_time < start_time {
            return (0, 0, 0, 0);
        }
        let mut motion_sum = 0i64;
        let mut obj_sum = 0i64;
        let mut reg_sum = 0i64;
        let mut peak_dbfs = 0i64;
        for s in &self.samples {
            if s.timestamp.is_finite() && s.timestamp >= start_time && s.timestamp <= end_time {
                motion_sum = motion_sum.saturating_add(s.motion_px as i64);
                obj_sum = obj_sum.saturating_add(s.active_objects as i64);
                reg_sum = reg_sum.saturating_add(s.regions as i64);
                if s.dbfs != 0 && (peak_dbfs == 0 || s.dbfs > peak_dbfs) {
                    peak_dbfs = s.dbfs;
                }
            }
        }
        (motion_sum, obj_sum, reg_sum, peak_dbfs)
    }
}

/// Validates a configured retention window in days, falling back to `default_days` when
/// `days` is non-finite (`NaN`, `+Inf`, `-Inf`) or `<= 0.0`, and clamping to `[min_days, MAX_RETENTION_DAYS]`.
pub fn sanitize_retention_days(days: f64, default_days: f64, min_days: f64) -> f64 {
    let safe_min = if min_days.is_finite() && min_days > 0.0 {
        min_days.min(MAX_RETENTION_DAYS)
    } else {
        0.1
    };
    let safe_default = if default_days.is_finite() && default_days > 0.0 {
        default_days.clamp(safe_min, MAX_RETENTION_DAYS)
    } else {
        7.0_f64.clamp(safe_min, MAX_RETENTION_DAYS)
    };
    if days.is_finite() && days > 0.0 {
        days.clamp(safe_min, MAX_RETENTION_DAYS)
    } else {
        safe_default
    }
}

/// Validates a configured storage quota or headroom threshold in GB, returning `None` when
/// non-finite, `<= 0.0`, or overflowing `val * 1024.0`, and capping to `MAX_STORAGE_QUOTA_GB`.
pub fn sanitize_quota_gb(val: Option<f64>) -> Option<f64> {
    let v = val?;
    if v.is_finite() && v > 0.0 && (v * 1024.0).is_finite() {
        Some(v.min(MAX_STORAGE_QUOTA_GB))
    } else {
        None
    }
}

/// Computes the allowed recording storage footprint in MB when free disk space drops below
/// `min_free_gb`, returning `None` if any input is non-finite, out of range, or when sufficient
/// free space is already available.
pub fn compute_headroom_allowed_mb(
    current_rec_mb: f64,
    free_mb: f64,
    min_free_gb: f64,
) -> Option<f64> {
    if !min_free_gb.is_finite()
        || min_free_gb <= 0.0
        || min_free_gb > MAX_STORAGE_QUOTA_GB
        || !(min_free_gb * 1024.0).is_finite()
    {
        return None;
    }
    if !current_rec_mb.is_finite() || current_rec_mb < 0.0 || !free_mb.is_finite() || free_mb < 0.0
    {
        return None;
    }
    let min_free_mb = min_free_gb * 1024.0;
    if !min_free_mb.is_finite() || free_mb >= min_free_mb {
        return None;
    }
    let deficit_mb = min_free_mb - free_mb;
    if !deficit_mb.is_finite() || deficit_mb <= 0.0 {
        return None;
    }
    let raw_allowed = current_rec_mb - deficit_mb;
    if !raw_allowed.is_finite() {
        return None;
    }
    Some(raw_allowed.max(0.1))
}

pub type SharedActivityLedger = Arc<RwLock<HashMap<String, CameraActivityRing>>>;

pub fn new_activity_ledger() -> SharedActivityLedger {
    Arc::new(RwLock::new(HashMap::new()))
}

fn unix_now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn random_suffix() -> String {
    let n: u32 = rand::random();
    format!("{n:06x}")
}

const MAX_MP4_BOXES_PER_LEVEL: usize = 4_096;

#[derive(Debug, Clone, Copy)]
struct MediaBoxSlice<'a> {
    box_type: [u8; 4],
    payload: &'a [u8],
}

fn iter_mp4_boxes(mut buf: &[u8]) -> (Vec<MediaBoxSlice<'_>>, bool) {
    let mut out = Vec::new();
    let mut malformed = false;
    while !buf.is_empty() {
        if out.len() >= MAX_MP4_BOXES_PER_LEVEL || buf.len() < 8 {
            malformed = true;
            break;
        }
        let size32 = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as u64;
        let box_type = [buf[4], buf[5], buf[6], buf[7]];
        let (box_len_u64, hdr_len) = if size32 == 1 {
            if buf.len() < 16 {
                malformed = true;
                break;
            }
            let size64 = u64::from_be_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]);
            (size64, 16usize)
        } else if size32 == 0 {
            (buf.len() as u64, 8usize)
        } else {
            (size32, 8usize)
        };
        let Ok(box_len) = usize::try_from(box_len_u64) else {
            malformed = true;
            break;
        };
        if box_len < hdr_len || box_len > buf.len() {
            malformed = true;
            break;
        }
        out.push(MediaBoxSlice {
            box_type,
            payload: &buf[hdr_len..box_len],
        });
        buf = &buf[box_len..];
    }
    (out, malformed)
}

fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && !s.starts_with('-')
        && !s.contains("..")
        && s.chars()
            .all(|c| !matches!(c, '/' | '\\' | '\0' | '\n' | '\r') && !c.is_control())
}

fn sanitize_ffmpeg_path_arg(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if path.is_relative()
        && (raw.starts_with('-')
            || raw
                .find(':')
                .is_some_and(|colon| raw.find('/').is_none_or(|slash| colon < slash)))
    {
        format!("./{raw}")
    } else {
        raw.into_owned()
    }
}

fn format_ffmpeg_concat_file_line(path: &Path) -> Option<String> {
    let raw = path.to_str()?;
    if raw.is_empty() || raw.contains(['\n', '\r', '\0']) {
        return None;
    }
    let escaped = raw.replace('\'', "'\\''");
    Some(format!("file '{escaped}'\n"))
}

struct TempPathGuard(PathBuf);

impl Drop for TempPathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
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
    if secs.is_finite() && secs > 0.0 && secs <= 300.0 {
        Some(secs)
    } else {
        None
    }
}

/// Reads the exact video track duration in seconds from an MP4 file's `vide` `mdhd` box
/// (falling back to `mvhd` if no `vide` track is present) in pure Rust without spawning `ffprobe`.
pub fn probe_mp4_duration_fast(path: &Path) -> Option<f64> {
    use std::io::{Read, Seek, SeekFrom};
    let (mut file, meta) = open_regular_file_nofollow(path).ok()?;
    let file_len = meta.len();
    let mut pos = 0u64;
    let mut hdr = [0u8; 8];
    let mut boxes_seen = 0usize;
    while pos.checked_add(8).is_some_and(|end| end <= file_len) {
        if boxes_seen >= MAX_MP4_BOXES_PER_LEVEL {
            break;
        }
        boxes_seen += 1;
        file.seek(SeekFrom::Start(pos)).ok()?;
        file.read_exact(&mut hdr).ok()?;
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let btype = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let (box_size, hdr_len) = if size32 == 1 {
            if pos.checked_add(16).is_none_or(|end| end > file_len) {
                break;
            }
            let mut ext = [0u8; 8];
            file.read_exact(&mut ext).ok()?;
            (u64::from_be_bytes(ext), 16u64)
        } else if size32 == 0 {
            (file_len - pos, 8u64)
        } else {
            (size32, 8u64)
        };
        let Some(next_pos) = pos.checked_add(box_size) else {
            break;
        };
        if box_size < hdr_len || next_pos > file_len {
            break;
        }
        if &btype == b"moov" {
            if box_size - hdr_len > 16 * 1024 * 1024 {
                break;
            }
            let payload_len = (box_size - hdr_len) as usize;
            let mut moov = vec![0u8; payload_len];
            file.read_exact(&mut moov).ok()?;
            let mut fallback_mvhd = None;
            let mut saw_vide_trak = false;
            let (moov_boxes, _) = iter_mp4_boxes(&moov);
            for b in moov_boxes {
                if &b.box_type == b"mvhd" {
                    fallback_mvhd = parse_duration_box(b.payload);
                } else if &b.box_type == b"trak" {
                    if let Some(vide_dur) = extract_vide_mdhd_duration(b.payload) {
                        return Some(vide_dur);
                    }
                    if trak_has_vide_handler(b.payload) {
                        saw_vide_trak = true;
                    }
                }
            }
            return if saw_vide_trak { None } else { fallback_mvhd };
        }
        pos = next_pos;
    }
    None
}

fn trak_has_vide_handler(trak_body: &[u8]) -> bool {
    let (trak_boxes, _) = iter_mp4_boxes(trak_body);
    for tb in trak_boxes {
        if &tb.box_type == b"mdia" {
            let (mdia_boxes, _) = iter_mp4_boxes(tb.payload);
            for db in mdia_boxes {
                if &db.box_type == b"hdlr"
                    && db.payload.len() >= 12
                    && &db.payload[8..12] == b"vide"
                {
                    return true;
                }
            }
        }
    }
    false
}

fn extract_vide_mdhd_duration(trak_body: &[u8]) -> Option<f64> {
    let (trak_boxes, _) = iter_mp4_boxes(trak_body);
    for tb in trak_boxes {
        if &tb.box_type == b"mdia" {
            let mut mdhd_dur = None;
            let mut is_vide = false;
            let (mdia_boxes, _) = iter_mp4_boxes(tb.payload);
            for db in mdia_boxes {
                if &db.box_type == b"mdhd" {
                    mdhd_dur = parse_duration_box(db.payload);
                } else if &db.box_type == b"hdlr"
                    && db.payload.len() >= 12
                    && &db.payload[8..12] == b"vide"
                {
                    is_vide = true;
                }
            }
            if is_vide && mdhd_dur.is_some() {
                return mdhd_dur;
            }
        }
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

const MAX_AUDIO_PROBE_PCM_BYTES: usize = 256 * 1024;
const AUDIO_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_IN_PROCESS_MP4_BYTES: u64 = 256 * 1024 * 1024;
const MAX_IN_PROCESS_AUDIO_PACKETS: usize = 1_024;
const MAX_IN_PROCESS_DEMUX_PACKETS: usize = 4_096;
const MAX_IN_PROCESS_PACKET_BYTES: usize = 65_536;
const MAX_IN_PROCESS_AUDIO_SECONDS: u64 = 15;
const MAX_SOUN_SPANS: usize = 2_048;
const MAX_SOUN_TABLE_ENTRIES: usize = 65_536;

fn rms_sum_to_dbfs(sum_sq: f64, sample_count: u64) -> Option<i64> {
    if sample_count == 0 {
        return None;
    }
    let rms = (sum_sq / (sample_count as f64)).sqrt();
    if !rms.is_finite() || rms <= 0.00005 {
        return Some(0);
    }
    let db = (20.0 * rms.log10()).round() as i64;
    Some(if db <= -90 { 0 } else { db.clamp(-89, -1) })
}

/// Decodes raw AAC-LC access units directly from validated `soun` `(offset, byte_len)` spans
/// without demuxing or reading any `vide` track samples from disk.
fn decode_aac_spans_in_process(
    file: &mut std::fs::File,
    file_len: u64,
    spans: &[(u64, u32)],
    sample_rate: u32,
    asc: &[u8],
) -> Option<i64> {
    use std::io::{Read, Seek, SeekFrom};
    use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
    use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
    use symphonia::core::errors::Error as SymphoniaError;
    use symphonia::core::packet::Packet;
    use symphonia::core::units::{Duration as SymphoniaDuration, Timestamp};

    if spans.is_empty() || asc.len() < 2 || asc.len() > 64 {
        return None;
    }
    let clamped_sr = sample_rate.clamp(4_000, 96_000);
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_AAC)
        .with_sample_rate(clamped_sr)
        .with_extra_data(asc.to_vec().into_boxed_slice());

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .ok()?;

    let max_frames = (clamped_sr as u64).saturating_mul(MAX_IN_PROCESS_AUDIO_SECONDS);
    let deadline = std::time::Instant::now() + AUDIO_PROBE_TIMEOUT;
    let mut frames_decoded: u64 = 0;
    let mut sum_sq: f64 = 0.0;
    let mut sample_count: u64 = 0;
    let mut sample_buf: Vec<i16> = Vec::with_capacity(2_048);

    for &(off, sz) in spans.iter().take(MAX_IN_PROCESS_AUDIO_PACKETS) {
        if frames_decoded >= max_frames || std::time::Instant::now() >= deadline {
            break;
        }
        let pkt_len = sz as usize;
        if pkt_len == 0 || pkt_len > MAX_IN_PROCESS_PACKET_BYTES {
            continue;
        }
        if off.checked_add(sz as u64).is_none_or(|end| end > file_len) {
            continue;
        }
        if file.seek(SeekFrom::Start(off)).is_err() {
            break;
        }
        let mut pkt_bytes = vec![0u8; pkt_len];
        if file.read_exact(&mut pkt_bytes).is_err() {
            break;
        }
        let pkt = Packet::new(0, Timestamp::ZERO, SymphoniaDuration::ZERO, pkt_bytes);
        match decoder.decode(&pkt) {
            Ok(decoded) => {
                let pkt_frames = decoded.frames() as u64;
                if pkt_frames == 0 {
                    continue;
                }
                frames_decoded = frames_decoded.saturating_add(pkt_frames);
                sample_buf.clear();
                decoded.copy_to_vec_interleaved::<i16>(&mut sample_buf);
                for &s in &sample_buf {
                    let norm = (s as f64) / 32768.0;
                    sum_sq += norm * norm;
                }
                sample_count = sample_count.saturating_add(sample_buf.len() as u64);
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    rms_sum_to_dbfs(sum_sq, sample_count)
}

/// Rejects fragmented MP4s (`mvex`) or pathological `moov` containers with excessive tracks,
/// malformed/truncated nested boxes, or multi-million-entry sample tables before passing a file
/// to `symphonia-format-isomp4`.
fn moov_is_safe_for_in_process_demux(moov: &[u8]) -> bool {
    let (moov_boxes, malformed) = iter_mp4_boxes(moov);
    if malformed {
        return false;
    }
    let mut trak_count = 0usize;
    for mb in moov_boxes {
        if &mb.box_type == b"mvex" {
            return false;
        }
        if &mb.box_type == b"trak" {
            trak_count += 1;
            if trak_count > 8 {
                return false;
            }
            let (trak_boxes, trak_malformed) = iter_mp4_boxes(mb.payload);
            if trak_malformed {
                return false;
            }
            for tb in trak_boxes {
                if &tb.box_type == b"mdia" {
                    let (mdia_boxes, mdia_malformed) = iter_mp4_boxes(tb.payload);
                    if mdia_malformed {
                        return false;
                    }
                    for db in mdia_boxes {
                        if &db.box_type == b"minf" {
                            let (minf_boxes, minf_malformed) = iter_mp4_boxes(db.payload);
                            if minf_malformed {
                                return false;
                            }
                            for ib in minf_boxes {
                                if &ib.box_type == b"stbl" {
                                    let (stbl_boxes, stbl_malformed) = iter_mp4_boxes(ib.payload);
                                    if stbl_malformed {
                                        return false;
                                    }
                                    for sb in stbl_boxes {
                                        let sbody = sb.payload;
                                        match &sb.box_type {
                                            b"stsz" if sbody.len() >= 12 => {
                                                let cnt = u32::from_be_bytes([
                                                    sbody[8], sbody[9], sbody[10], sbody[11],
                                                ])
                                                    as usize;
                                                if cnt > MAX_SOUN_TABLE_ENTRIES {
                                                    return false;
                                                }
                                            }
                                            b"stts" | b"ctts" | b"stsc" | b"stco" | b"co64"
                                                if sbody.len() >= 8 =>
                                            {
                                                let cnt = u32::from_be_bytes([
                                                    sbody[4], sbody[5], sbody[6], sbody[7],
                                                ])
                                                    as usize;
                                                if cnt > MAX_SOUN_TABLE_ENTRIES {
                                                    return false;
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    trak_count > 0
}

/// Scans all top-level ISO-BMFF box headers from byte `0` to `file_len` and returns `true` only
/// when the container is cleanly bounded, has **exactly one** `moov` box, and has **zero** `moof`
/// boxes (preventing second-`moov` overwrite or top-level `moof`/`trun` smuggling).
fn file_has_single_moov_and_no_moof(file: &mut std::fs::File, file_len: u64) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    let mut pos = 0u64;
    let mut hdr = [0u8; 8];
    let mut boxes_seen = 0usize;
    let mut moov_count = 0usize;
    while pos < file_len {
        if boxes_seen >= MAX_MP4_BOXES_PER_LEVEL || pos.checked_add(8).is_none_or(|e| e > file_len)
        {
            return false;
        }
        boxes_seen += 1;
        if file.seek(SeekFrom::Start(pos)).is_err() || file.read_exact(&mut hdr).is_err() {
            return false;
        }
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let btype = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let (box_size, hdr_len) = if size32 == 1 {
            if pos.checked_add(16).is_none_or(|e| e > file_len) {
                return false;
            }
            let mut ext = [0u8; 8];
            if file.read_exact(&mut ext).is_err() {
                return false;
            }
            (u64::from_be_bytes(ext), 16u64)
        } else if size32 == 0 {
            (file_len - pos, 8u64)
        } else {
            (size32, 8u64)
        };
        let Some(next_pos) = pos.checked_add(box_size) else {
            return false;
        };
        if box_size < hdr_len || next_pos > file_len {
            return false;
        }
        if &btype == b"moof" {
            return false;
        }
        if &btype == b"moov" {
            moov_count += 1;
            if moov_count > 1 || box_size - hdr_len > 16 * 1024 * 1024 {
                return false;
            }
        }
        pos = next_pos;
    }
    pos == file_len && moov_count == 1
}

/// Decodes an MP4 `soun` track (`mp4a` AAC-LC or linear PCM) in-process
/// using pure-Rust `symphonia` (`#![forbid(unsafe_code)]`) without spawning `ffmpeg`.
/// Returns `Some(dbfs)` when at least one valid audio sample was decoded, or `None` when
/// the codec/container is unsupported or malformed (allowing optional `ffmpeg` fallback).
fn decode_mp4_audio_dbfs_in_process(path: &Path) -> Option<i64> {
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::common::Limit;
    use symphonia::core::errors::Error as SymphoniaError;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;

    let (file, meta) = open_regular_file_nofollow(path).ok()?;
    if meta.len() > MAX_IN_PROCESS_MP4_BYTES {
        return None;
    }
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    hint.with_extension("mp4");

    let meta_opts = MetadataOptions::default()
        .limit_tag_bytes(Limit::Maximum(4_096))
        .limit_visual_bytes(Limit::Maximum(0));

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), meta_opts)
        .ok()?;

    let (track_id, audio_params, sample_rate) = {
        let track = format.default_track(TrackType::Audio)?;
        let Some(CodecParameters::Audio(params)) = &track.codec_params else {
            return None;
        };
        let sr = params.sample_rate.unwrap_or(16_000).clamp(4_000, 96_000) as u64;
        (track.id, params.clone(), sr)
    };

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .ok()?;

    let max_frames = sample_rate.saturating_mul(MAX_IN_PROCESS_AUDIO_SECONDS);
    let deadline = std::time::Instant::now() + AUDIO_PROBE_TIMEOUT;
    let mut frames_decoded: u64 = 0;
    let mut audio_packets_seen: usize = 0;
    let mut total_packets_seen: usize = 0;
    let mut sum_sq: f64 = 0.0;
    let mut sample_count: u64 = 0;
    let mut sample_buf: Vec<i16> = Vec::with_capacity(2_048);

    while total_packets_seen < MAX_IN_PROCESS_DEMUX_PACKETS
        && audio_packets_seen < MAX_IN_PROCESS_AUDIO_PACKETS
        && frames_decoded < max_frames
        && std::time::Instant::now() < deadline
    {
        let packet = match format.next_packet() {
            Ok(Some(pkt)) => pkt,
            Ok(None) => break,
            Err(SymphoniaError::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(_) => break,
        };
        total_packets_seen += 1;
        if packet.track_id != track_id {
            continue;
        }
        if packet.data.len() > MAX_IN_PROCESS_PACKET_BYTES {
            break;
        }
        audio_packets_seen += 1;

        match decoder.decode(&packet) {
            Ok(decoded) => {
                let pkt_frames = decoded.frames() as u64;
                if pkt_frames == 0 {
                    continue;
                }
                frames_decoded = frames_decoded.saturating_add(pkt_frames);
                sample_buf.clear();
                decoded.copy_to_vec_interleaved::<i16>(&mut sample_buf);
                for &s in &sample_buf {
                    let norm = (s as f64) / 32768.0;
                    sum_sq += norm * norm;
                }
                sample_count = sample_count.saturating_add(sample_buf.len() as u64);
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    rms_sum_to_dbfs(sum_sq, sample_count)
}

fn decode_audio_dbfs_with_bin_and_timeout(
    ffmpeg_bin: &Path,
    path: &Path,
    timeout: std::time::Duration,
) -> i64 {
    use std::io::Read;

    let safe_input = sanitize_ffmpeg_path_arg(path);
    let mut cmd = std::process::Command::new(ffmpeg_bin);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ])
    .arg(safe_input)
    .args([
        "-t", "15", "-vn", "-ac", "1", "-ar", "8000", "-f", "s16le", "pipe:1",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());

    let Ok(mut child) = cmd.spawn() else {
        return 0;
    };
    let child_pid = child.id();

    let kill_probe_tree = |c: &mut std::process::Child| {
        #[cfg(unix)]
        if let Ok(raw_pid) = i32::try_from(child_pid)
            && raw_pid > 1
        {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(raw_pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = c.kill();
        let _ = c.wait();
    };

    let reader_handle = child.stdout.take().map(|stdout| {
        std::thread::spawn(move || {
            let mut buf = Vec::with_capacity(64 * 1024);
            let _ = stdout
                .take(MAX_AUDIO_PROBE_PCM_BYTES as u64)
                .read_to_end(&mut buf);
            buf
        })
    });

    let deadline = std::time::Instant::now() + timeout;
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    kill_probe_tree(&mut child);
                    if let Some(h) = reader_handle {
                        let _ = h.join();
                    }
                    return 0;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => {
                kill_probe_tree(&mut child);
                if let Some(h) = reader_handle {
                    let _ = h.join();
                }
                return 0;
            }
        }
    };

    let raw_pcm = reader_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    if !exit_status.is_some_and(|s| s.success()) && raw_pcm.len() < MAX_AUDIO_PROBE_PCM_BYTES {
        return 0;
    }
    if raw_pcm.len() < 4 {
        return 0;
    }
    let pcm_samples: Vec<i16> = raw_pcm
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(i16::from_le_bytes)
        .collect();
    compute_pcm_rms_dbfs(&pcm_samples)
}

fn decode_audio_dbfs_via_ffmpeg(path: &Path) -> i64 {
    decode_audio_dbfs_with_bin_and_timeout(&resolve_ffmpeg_bin(), path, AUDIO_PROBE_TIMEOUT)
}

/// Probes an MP4 segment file to extract the acoustic RMS level in dBFS
/// (`-89`..`-1` for active audio, `0` if silent or no `soun` track).
///
/// First inspects the ISO-BMFF `moov` -> `soun` track in pure Rust (`< 0.05ms`):
/// - If there is no `soun` track or `sample_count == 0`, returns `0` immediately without spawning FFmpeg.
/// - If `stsd` indicates a compressed codec (`mp4a` AAC, `alaw`, `ulaw`, `Opus`, etc.), decodes the
///   audio track to 8kHz mono `s16le` PCM via `ffmpeg` (bounded to `5s` and `256 KiB`) only when
///   `decode_compressed` is `true`.
/// - If `stsd` is uncompressed PCM (or omitted in synthetic test containers), reads raw `s16le` samples
///   directly from the `mdat` chunk offsets.
pub fn probe_mp4_audio_dbfs_fast(path: &Path) -> i64 {
    probe_mp4_audio_dbfs_with_policy(path, true)
}

/// Policy-gated variant of [`probe_mp4_audio_dbfs_fast`] that skips spawning `ffmpeg` for
/// compressed audio codecs when `decode_compressed` is `false`.
pub fn probe_mp4_audio_dbfs_with_policy(path: &Path, decode_compressed: bool) -> i64 {
    probe_mp4_audio_dbfs_with_bin(path, decode_compressed, None)
}

fn decode_g711_alaw_sample(alaw: u8) -> i16 {
    let val = alaw ^ 0x55;
    let sign = val & 0x80;
    let exponent = (val >> 4) & 0x07;
    let mantissa = (val & 0x0F) as i32;
    let mag = if exponent == 0 {
        (mantissa << 4) + 8
    } else {
        ((mantissa << 4) + 0x108) << (exponent - 1)
    };
    if sign != 0 { mag as i16 } else { -(mag as i16) }
}

fn decode_g711_ulaw_sample(ulaw: u8) -> i16 {
    let val = !ulaw;
    let sign = val & 0x80;
    let exponent = (val >> 4) & 0x07;
    let mantissa = (val & 0x0F) as i32;
    let mag = (((mantissa << 3) + 0x84) << exponent) - 0x84;
    if sign != 0 { -(mag as i16) } else { mag as i16 }
}

fn probe_mp4_audio_dbfs_with_bin(
    path: &Path,
    decode_compressed: bool,
    ffmpeg_bin_override: Option<&Path>,
) -> i64 {
    use std::io::{Read, Seek, SeekFrom};
    let Ok((mut file, meta)) = open_regular_file_nofollow(path) else {
        return 0;
    };
    let file_len = meta.len();
    let mut pos = 0u64;
    let mut hdr = [0u8; 8];
    let mut boxes_seen = 0usize;
    while pos.checked_add(8).is_some_and(|end| end <= file_len) {
        if boxes_seen >= MAX_MP4_BOXES_PER_LEVEL {
            break;
        }
        boxes_seen += 1;
        if file.seek(SeekFrom::Start(pos)).is_err() || file.read_exact(&mut hdr).is_err() {
            break;
        }
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let btype = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let (box_size, hdr_len) = if size32 == 1 {
            if pos.checked_add(16).is_none_or(|end| end > file_len) {
                break;
            }
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
        let Some(next_pos) = pos.checked_add(box_size) else {
            break;
        };
        if box_size < hdr_len || next_pos > file_len {
            break;
        }
        if &btype == b"moov" {
            if box_size - hdr_len > 16 * 1024 * 1024 {
                break;
            }
            let mut moov = vec![0u8; (box_size - hdr_len) as usize];
            if file.read_exact(&mut moov).is_err() {
                break;
            }
            if let Some(stats) = extract_soun_sample_stats(&moov) {
                let SounSampleStats {
                    spans,
                    total_bytes: total_audio_bytes,
                    sample_count,
                    is_compressed,
                    codec_fourcc: fourcc,
                    sample_rate,
                    aac_asc,
                } = stats;
                if sample_count == 0 || total_audio_bytes == 0 {
                    return 0;
                }
                if is_compressed {
                    if !decode_compressed {
                        return 0;
                    }
                    if matches!(&fourcc, b"alaw" | b"ulaw") {
                        const MAX_G711_READ_BYTES: usize = 128 * 1024;
                        let mut g711_bytes = Vec::with_capacity(16_384);
                        for (off, span_len) in spans {
                            if g711_bytes.len() as u64 >= total_audio_bytes
                                || g711_bytes.len() >= MAX_G711_READ_BYTES
                            {
                                break;
                            }
                            if span_len == 0 || off >= file_len {
                                continue;
                            }
                            if file.seek(SeekFrom::Start(off)).is_ok() {
                                let rem_cap = (MAX_G711_READ_BYTES - g711_bytes.len()) as u64;
                                let rem_tot =
                                    total_audio_bytes.saturating_sub(g711_bytes.len() as u64);
                                let to_read = (span_len as u64)
                                    .min(file_len - off)
                                    .min(rem_cap)
                                    .min(rem_tot)
                                    as usize;
                                if to_read == 0 {
                                    continue;
                                }
                                let mut chunk = vec![0u8; to_read];
                                if file.read_exact(&mut chunk).is_ok() {
                                    g711_bytes.extend_from_slice(&chunk);
                                }
                            }
                        }
                        if g711_bytes.is_empty() {
                            return 0;
                        }
                        let is_alaw = &fourcc == b"alaw";
                        let pcm_samples: Vec<i16> = g711_bytes
                            .into_iter()
                            .map(|b| {
                                if is_alaw {
                                    decode_g711_alaw_sample(b)
                                } else {
                                    decode_g711_ulaw_sample(b)
                                }
                            })
                            .collect();
                        return compute_pcm_rms_dbfs(&pcm_samples);
                    }
                    if &fourcc == b"mp4a" {
                        if let Some(ref asc) = aac_asc
                            && let Some(direct_db) = decode_aac_spans_in_process(
                                &mut file,
                                file_len,
                                &spans,
                                sample_rate,
                                asc,
                            )
                        {
                            return direct_db;
                        }
                        if moov_is_safe_for_in_process_demux(&moov)
                            && file_has_single_moov_and_no_moof(&mut file, file_len)
                            && let Some(in_proc_db) = decode_mp4_audio_dbfs_in_process(path)
                        {
                            return in_proc_db;
                        }
                    }
                    return match ffmpeg_bin_override {
                        Some(bin) => {
                            decode_audio_dbfs_with_bin_and_timeout(bin, path, AUDIO_PROBE_TIMEOUT)
                        }
                        None => decode_audio_dbfs_via_ffmpeg(path),
                    };
                }
                const MAX_PCM_READ_BYTES: usize = 32 * 1024;
                let mut payload = Vec::with_capacity(8192);
                for (off, span_len) in spans {
                    if payload.len() as u64 >= total_audio_bytes
                        || payload.len() >= MAX_PCM_READ_BYTES
                    {
                        break;
                    }
                    if span_len == 0 || off >= file_len {
                        continue;
                    }
                    if file.seek(SeekFrom::Start(off)).is_ok() {
                        let rem_cap = (MAX_PCM_READ_BYTES - payload.len()) as u64;
                        let remaining_audio = total_audio_bytes
                            .saturating_sub(payload.len() as u64)
                            .min(rem_cap);
                        let to_read =
                            (span_len as u64).min(file_len - off).min(remaining_audio) as usize;
                        if to_read == 0 {
                            continue;
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
        pos = next_pos;
    }
    0
}

fn is_known_iso_top_level_box(btype: &[u8; 4]) -> bool {
    matches!(
        btype,
        b"ftyp"
            | b"styp"
            | b"moov"
            | b"moof"
            | b"mdat"
            | b"free"
            | b"skip"
            | b"wide"
            | b"uuid"
            | b"pdin"
            | b"meta"
            | b"sidx"
            | b"ssix"
            | b"prft"
            | b"emsg"
            | b"mfra"
    )
}

/// Fast pure-Rust ISO-BMFF `moov` inspection helper that returns `true` when the MP4 container
/// carries an audio (`soun`) track with non-empty samples (or a `soun` `trak` box).
pub fn mp4_file_has_audio_track(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(lmeta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !lmeta.file_type().is_file() {
        return true;
    }
    let Ok((mut file, fd_meta)) = open_regular_file_nofollow(path) else {
        return true;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if fd_meta.dev() != lmeta.dev() || fd_meta.ino() != lmeta.ino() {
            return true;
        }
    }
    let file_len = fd_meta.len();
    if file_len < 8 {
        return false;
    }
    let mut pos = 0u64;
    let mut hdr = [0u8; 8];
    let mut boxes_seen = 0usize;
    while pos.checked_add(8).is_some_and(|end| end <= file_len) {
        if boxes_seen >= MAX_MP4_BOXES_PER_LEVEL {
            return true;
        }
        boxes_seen += 1;
        if file.seek(SeekFrom::Start(pos)).is_err() || file.read_exact(&mut hdr).is_err() {
            return true;
        }
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let btype = [hdr[4], hdr[5], hdr[6], hdr[7]];
        if boxes_seen == 1 && !is_known_iso_top_level_box(&btype) {
            return false;
        }
        let (box_size, hdr_len) = if size32 == 1 {
            if pos.checked_add(16).is_none_or(|end| end > file_len) {
                return true;
            }
            let mut ext = [0u8; 8];
            if file.read_exact(&mut ext).is_err() {
                return true;
            }
            (u64::from_be_bytes(ext), 16u64)
        } else if size32 == 0 {
            (file_len - pos, 8u64)
        } else {
            (size32, 8u64)
        };
        let Some(next_pos) = pos.checked_add(box_size) else {
            return true;
        };
        if box_size < hdr_len || next_pos > file_len || &btype == b"moof" {
            return true;
        }
        if &btype == b"moov" {
            if box_size - hdr_len > 16 * 1024 * 1024 {
                return true;
            }
            let mut moov = vec![0u8; (box_size - hdr_len) as usize];
            if file.read_exact(&mut moov).is_err() {
                return true;
            }
            if moov_has_soun_trak(&moov) {
                return true;
            }
        }
        pos = next_pos;
    }
    pos != file_len
}

fn moov_has_soun_trak(moov: &[u8]) -> bool {
    let (moov_boxes, malformed_moov) = iter_mp4_boxes(moov);
    if malformed_moov {
        return true;
    }
    let has_mvex = moov_boxes.iter().any(|b| &b.box_type == b"mvex");
    for mb in moov_boxes {
        if &mb.box_type == b"trak" {
            let (trak_boxes, malformed_trak) = iter_mp4_boxes(mb.payload);
            if malformed_trak {
                return true;
            }
            for tb in trak_boxes {
                if &tb.box_type == b"mdia" {
                    let (mdia_boxes, malformed_mdia) = iter_mp4_boxes(tb.payload);
                    if malformed_mdia {
                        return true;
                    }
                    let mut saw_hdlr = false;
                    let mut is_soun = false;
                    let mut minf_payloads: Vec<&[u8]> = Vec::new();
                    for db in mdia_boxes {
                        if &db.box_type == b"hdlr" {
                            if db.payload.len() < 12 {
                                return true;
                            }
                            saw_hdlr = true;
                            if &db.payload[8..12] == b"soun" {
                                is_soun = true;
                            }
                        } else if &db.box_type == b"minf" {
                            minf_payloads.push(db.payload);
                        }
                    }
                    if !saw_hdlr {
                        return true;
                    }
                    if !is_soun {
                        continue;
                    }
                    if has_mvex || minf_payloads.is_empty() {
                        return true;
                    }
                    for minf in minf_payloads {
                        let (minf_boxes, malformed_minf) = iter_mp4_boxes(minf);
                        if malformed_minf {
                            return true;
                        }
                        let stbl_payloads: Vec<&[u8]> = minf_boxes
                            .iter()
                            .filter(|b| &b.box_type == b"stbl")
                            .map(|b| b.payload)
                            .collect();
                        if stbl_payloads.is_empty() {
                            return true;
                        }
                        for stbl in stbl_payloads {
                            let (stbl_boxes, malformed_stbl) = iter_mp4_boxes(stbl);
                            if malformed_stbl || stbl_boxes.iter().any(|b| &b.box_type == b"stz2") {
                                return true;
                            }
                            let stsz_payloads: Vec<&[u8]> = stbl_boxes
                                .iter()
                                .filter(|b| &b.box_type == b"stsz")
                                .map(|b| b.payload)
                                .collect();
                            if stsz_payloads.is_empty() {
                                return true;
                            }
                            for stsz in stsz_payloads {
                                if stsz.len() < 12 {
                                    return true;
                                }
                                let default_size =
                                    u32::from_be_bytes([stsz[4], stsz[5], stsz[6], stsz[7]]);
                                let count =
                                    u32::from_be_bytes([stsz[8], stsz[9], stsz[10], stsz[11]]);
                                if count == 0 {
                                    continue;
                                }
                                if default_size > 0 {
                                    return true;
                                }
                                let needed =
                                    12usize.saturating_add((count as usize).saturating_mul(4));
                                if stsz.len() < needed {
                                    return true;
                                }
                                let mut total_bytes = 0u64;
                                for i in 0..(count as usize) {
                                    let p = 12 + i * 4;
                                    let sz = u32::from_be_bytes([
                                        stsz[p],
                                        stsz[p + 1],
                                        stsz[p + 2],
                                        stsz[p + 3],
                                    ]) as u64;
                                    total_bytes = total_bytes.saturating_add(sz);
                                }
                                if total_bytes > 0 {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

struct SounSampleStats {
    spans: Vec<(u64, u32)>,
    total_bytes: u64,
    sample_count: u32,
    is_compressed: bool,
    codec_fourcc: [u8; 4],
    sample_rate: u32,
    aac_asc: Option<Vec<u8>>,
}

fn read_mp4_descr_len(buf: &[u8], pos: &mut usize) -> Option<usize> {
    let mut len = 0usize;
    for _ in 0..4 {
        let &b = buf.get(*pos)?;
        *pos += 1;
        len = (len << 7) | ((b & 0x7F) as usize);
        if (b & 0x80) == 0 {
            return Some(len);
        }
    }
    None
}

fn extract_aac_asc_from_esds(esds_payload: &[u8]) -> Option<Vec<u8>> {
    if esds_payload.len() < 9 {
        return None;
    }
    let mut pos = 4usize; // skip FullBox version + flags
    while pos < esds_payload.len() {
        let tag = *esds_payload.get(pos)?;
        pos += 1;
        let len = read_mp4_descr_len(esds_payload, &mut pos)?;
        let end = pos.checked_add(len)?.min(esds_payload.len());
        if tag == 0x03 {
            // ES_Descriptor: ES_ID (2) + flags (1) + optional dependsOn/URL/OCR
            if pos + 3 > end {
                return None;
            }
            let flags = esds_payload[pos + 2];
            pos += 3;
            if (flags & 0x80) != 0 {
                pos = pos.checked_add(2)?;
            }
            if (flags & 0x40) != 0 {
                let url_len = *esds_payload.get(pos)? as usize;
                pos = pos.checked_add(1 + url_len)?;
            }
            if (flags & 0x20) != 0 {
                pos = pos.checked_add(2)?;
            }
        } else if tag == 0x04 {
            // DecoderConfigDescriptor: 13-byte header followed by DecSpecificInfo (0x05)
            if pos + 13 > end {
                return None;
            }
            let oti = esds_payload[pos];
            if !matches!(oti, 0x40 | 0x66 | 0x67 | 0x68) {
                return None; // Only MPEG-4 / MPEG-2 AAC Audio carries AudioSpecificConfig for AAC-LC
            }
            pos += 13;
        } else if tag == 0x05 {
            let asc = &esds_payload[pos..end];
            if (2..=64).contains(&asc.len()) {
                return Some(asc.to_vec());
            }
            return None;
        } else {
            pos = end;
        }
    }
    None
}

fn extract_soun_sample_stats(moov: &[u8]) -> Option<SounSampleStats> {
    let (moov_boxes, _) = iter_mp4_boxes(moov);
    let mut fallback_empty = None;
    for mb in moov_boxes {
        if &mb.box_type == b"trak"
            && let Some(stats) = parse_soun_trak(mb.payload)
        {
            if stats.total_bytes > 0 && stats.sample_count > 0 {
                return Some(stats);
            }
            if fallback_empty.is_none() {
                fallback_empty = Some(stats);
            }
        }
    }
    fallback_empty
}

fn parse_soun_trak(trak: &[u8]) -> Option<SounSampleStats> {
    let (trak_boxes, _) = iter_mp4_boxes(trak);
    for tb in trak_boxes {
        if &tb.box_type == b"mdia" {
            let mut is_soun = false;
            let mut mdhd_timescale = 0u32;
            let mut stbl_slice: Option<&[u8]> = None;
            let (mdia_boxes, _) = iter_mp4_boxes(tb.payload);
            for db in mdia_boxes {
                if &db.box_type == b"hdlr"
                    && db.payload.len() >= 12
                    && &db.payload[8..12] == b"soun"
                {
                    is_soun = true;
                } else if &db.box_type == b"mdhd" && db.payload.len() >= 20 {
                    let ver = db.payload[0];
                    if ver == 1 && db.payload.len() >= 24 {
                        mdhd_timescale = u32::from_be_bytes([
                            db.payload[20],
                            db.payload[21],
                            db.payload[22],
                            db.payload[23],
                        ]);
                    } else if ver == 0 {
                        mdhd_timescale = u32::from_be_bytes([
                            db.payload[12],
                            db.payload[13],
                            db.payload[14],
                            db.payload[15],
                        ]);
                    }
                } else if &db.box_type == b"minf" {
                    let (minf_boxes, _) = iter_mp4_boxes(db.payload);
                    for ib in minf_boxes {
                        if &ib.box_type == b"stbl" {
                            stbl_slice = Some(ib.payload);
                        }
                    }
                }
            }
            if is_soun && let Some(stbl) = stbl_slice {
                let mut chunk_offsets: Vec<u64> = Vec::new();
                let mut stsc_entries: Vec<(u32, u32)> = Vec::new();
                let mut sample_sizes: Vec<u32> = Vec::new();
                let mut default_sample_size = 0u32;
                let mut total_bytes = 0u64;
                let mut sample_count = 0u32;
                let mut is_compressed = false;
                let mut codec_fourcc = *b"NONE";
                let mut sample_rate = mdhd_timescale;
                let mut aac_asc: Option<Vec<u8>> = None;

                let (stbl_boxes, _) = iter_mp4_boxes(stbl);
                for sb in stbl_boxes {
                    let stype = &sb.box_type;
                    let sbody = sb.payload;
                    if stype == b"stsd" && sbody.len() >= 16 {
                        let fourcc = [sbody[12], sbody[13], sbody[14], sbody[15]];
                        codec_fourcc = fourcc;
                        is_compressed =
                            !matches!(&fourcc, b"sowt" | b"twos" | b"lpcm" | b"raw " | b"NONE");
                        let (entries, _) = iter_mp4_boxes(&sbody[8..]);
                        if let Some(entry) = entries.first() {
                            codec_fourcc = entry.box_type;
                            is_compressed = !matches!(
                                &codec_fourcc,
                                b"sowt" | b"twos" | b"lpcm" | b"raw " | b"NONE"
                            );
                            if entry.payload.len() >= 28 {
                                let sr_hi =
                                    u16::from_be_bytes([entry.payload[24], entry.payload[25]])
                                        as u32;
                                if sr_hi > 0 {
                                    sample_rate = sr_hi;
                                }
                                let qt_ver =
                                    u16::from_be_bytes([entry.payload[8], entry.payload[9]]);
                                let child_off = match qt_ver {
                                    1 => 44usize,
                                    2 => 64usize,
                                    _ => 28usize,
                                };
                                if &codec_fourcc == b"mp4a" && entry.payload.len() > child_off {
                                    let (children, _) = iter_mp4_boxes(&entry.payload[child_off..]);
                                    for ch in children {
                                        if &ch.box_type == b"esds" {
                                            aac_asc = extract_aac_asc_from_esds(ch.payload);
                                            if aac_asc.is_some() {
                                                break;
                                            }
                                        } else if &ch.box_type == b"wave" {
                                            let (wchildren, _) = iter_mp4_boxes(ch.payload);
                                            for wch in wchildren {
                                                if &wch.box_type == b"esds" {
                                                    aac_asc =
                                                        extract_aac_asc_from_esds(wch.payload);
                                                    if aac_asc.is_some() {
                                                        break;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else if stype == b"stsc" && sbody.len() >= 8 {
                        let count =
                            u32::from_be_bytes([sbody[4], sbody[5], sbody[6], sbody[7]]) as usize;
                        let mut bpos = 8usize;
                        for _ in 0..count.min(MAX_SOUN_TABLE_ENTRIES) {
                            if bpos + 12 > sbody.len() {
                                break;
                            }
                            let first_chunk = u32::from_be_bytes([
                                sbody[bpos],
                                sbody[bpos + 1],
                                sbody[bpos + 2],
                                sbody[bpos + 3],
                            ]);
                            let spc = u32::from_be_bytes([
                                sbody[bpos + 4],
                                sbody[bpos + 5],
                                sbody[bpos + 6],
                                sbody[bpos + 7],
                            ]);
                            if first_chunk > 0 {
                                stsc_entries.push((first_chunk, spc));
                            }
                            bpos += 12;
                        }
                    } else if stype == b"stsz" && sbody.len() >= 12 {
                        let default_size =
                            u32::from_be_bytes([sbody[4], sbody[5], sbody[6], sbody[7]]);
                        let count = u32::from_be_bytes([sbody[8], sbody[9], sbody[10], sbody[11]]);
                        default_sample_size = default_size;
                        sample_count = count;
                        if default_size > 0 {
                            total_bytes = (default_size as u64).saturating_mul(count as u64);
                        } else {
                            let cap = (count as usize).min(MAX_SOUN_TABLE_ENTRIES);
                            sample_sizes.reserve(cap.min(4_096));
                            let mut bpos = 12usize;
                            for _ in 0..cap {
                                if bpos + 4 > sbody.len() {
                                    break;
                                }
                                let sz = u32::from_be_bytes([
                                    sbody[bpos],
                                    sbody[bpos + 1],
                                    sbody[bpos + 2],
                                    sbody[bpos + 3],
                                ]);
                                sample_sizes.push(sz);
                                total_bytes = total_bytes.saturating_add(sz as u64);
                                bpos += 4;
                            }
                        }
                    } else if stype == b"stco" && sbody.len() >= 8 {
                        let count =
                            u32::from_be_bytes([sbody[4], sbody[5], sbody[6], sbody[7]]) as usize;
                        let mut bpos = 8usize;
                        for _ in 0..count.min(MAX_SOUN_SPANS) {
                            if bpos + 4 > sbody.len() {
                                break;
                            }
                            chunk_offsets.push(u32::from_be_bytes([
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
                        for _ in 0..count.min(MAX_SOUN_SPANS) {
                            if bpos + 8 > sbody.len() {
                                break;
                            }
                            chunk_offsets.push(u64::from_be_bytes([
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
                }

                // Expand exact `(offset, byte_len)` spans strictly belonging to the `soun` track:
                // - Packetized `mp4a` emits one span per access unit so `AacDecoder` decodes frame-by-frame;
                // - Byte-stream codecs (`alaw`, `ulaw`, PCM) coalesce contiguous samples per chunk so
                //   interleaved `vide` NALs in `mdat` are NEVER read past the chunk boundary.
                let packetized = matches!(&codec_fourcc, b"mp4a" | b"Opus");
                let mut spans: Vec<(u64, u32)> = Vec::new();
                let total_samples = if default_sample_size > 0 {
                    sample_count as usize
                } else {
                    sample_sizes.len()
                };

                if !chunk_offsets.is_empty() && total_samples > 0 {
                    let mut sample_idx = 0usize;
                    let mut stsc_cursor = 0usize;
                    let num_chunks = chunk_offsets.len();

                    for (c_idx, &chunk_off) in chunk_offsets.iter().enumerate() {
                        if sample_idx >= total_samples || spans.len() >= MAX_SOUN_SPANS {
                            break;
                        }
                        let chunk_num_1based = (c_idx + 1) as u32;
                        let spc = if stsc_entries.is_empty() {
                            if num_chunks == 1 {
                                total_samples
                            } else {
                                total_samples.div_ceil(num_chunks)
                            }
                        } else {
                            while stsc_cursor + 1 < stsc_entries.len()
                                && stsc_entries[stsc_cursor + 1].0 <= chunk_num_1based
                            {
                                stsc_cursor += 1;
                            }
                            stsc_entries[stsc_cursor].1 as usize
                        };
                        if spc == 0 {
                            continue;
                        }
                        let n_in_chunk = spc.min(total_samples - sample_idx);
                        if packetized {
                            let mut cur_off = chunk_off;
                            for _ in 0..n_in_chunk {
                                if spans.len() >= MAX_SOUN_SPANS {
                                    break;
                                }
                                let sz = if default_sample_size > 0 {
                                    default_sample_size
                                } else {
                                    sample_sizes[sample_idx]
                                };
                                sample_idx += 1;
                                if sz > 0 {
                                    spans.push((cur_off, sz));
                                }
                                let Some(next_off) = cur_off.checked_add(sz as u64) else {
                                    break;
                                };
                                cur_off = next_off;
                            }
                        } else if default_sample_size > 0 {
                            let chunk_bytes = (n_in_chunk as u64)
                                .saturating_mul(default_sample_size as u64)
                                .min(u32::MAX as u64)
                                as u32;
                            sample_idx += n_in_chunk;
                            if chunk_bytes > 0 {
                                spans.push((chunk_off, chunk_bytes));
                            }
                        } else {
                            let mut chunk_bytes = 0u64;
                            for _ in 0..n_in_chunk {
                                chunk_bytes =
                                    chunk_bytes.saturating_add(sample_sizes[sample_idx] as u64);
                                sample_idx += 1;
                            }
                            let clamped = chunk_bytes.min(u32::MAX as u64) as u32;
                            if clamped > 0 {
                                spans.push((chunk_off, clamped));
                            }
                        }
                    }
                }

                return Some(SounSampleStats {
                    spans,
                    total_bytes,
                    sample_count,
                    is_compressed,
                    codec_fourcc,
                    sample_rate: if sample_rate > 0 { sample_rate } else { 16_000 },
                    aac_asc,
                });
            }
        }
    }
    None
}

/// Parses the UTC start timestamp from a staged filename (`{camera}_{epoch_ms}_seg_{seq}.mp4`
/// or legacy `{camera}_%Y%m%d%H%M%S.mp4`).
fn parse_staged_start_time(camera: &str, staged_path: &Path) -> Option<f64> {
    let stem = staged_path.file_stem()?.to_str()?;
    let rest = stem.strip_prefix(&format!("{camera}_"))?;
    if let Some((epoch_str, seq_str)) = rest.split_once("_seg_") {
        let raw_epoch: u64 = epoch_str.parse().ok()?;
        let seq: u64 = seq_str.parse().ok()?;
        let base_sec = if raw_epoch >= 100_000_000_000 {
            (raw_epoch as f64) / 1000.0
        } else {
            raw_epoch as f64
        };
        return Some(base_sec + (seq as f64) * 10.0);
    }
    if rest.starts_with("seg_") {
        let modified = std::fs::symlink_metadata(staged_path)
            .ok()?
            .modified()
            .ok()?;
        let mtime_sec = modified.duration_since(UNIX_EPOCH).ok()?.as_secs_f64();
        return Some((mtime_sec - 10.0).max(0.0));
    }
    let ndt = NaiveDateTime::parse_from_str(rest, "%Y%m%d%H%M%S").ok()?;
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
    if !is_safe_identifier(camera) {
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
        let Ok(date_meta) = std::fs::symlink_metadata(&date_path) else {
            continue;
        };
        if date_name.starts_with('.') || !date_meta.file_type().is_dir() {
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
            let Ok(hour_meta) = std::fs::symlink_metadata(&hour_path) else {
                continue;
            };
            if hour_name.starts_with('.') || !hour_meta.file_type().is_dir() {
                continue;
            }
            let Ok(cam_entries) = std::fs::read_dir(&hour_path) else {
                continue;
            };
            for cam_entry in cam_entries.flatten() {
                let cam_path = cam_entry.path();
                let Ok(cam_meta) = std::fs::symlink_metadata(&cam_path) else {
                    continue;
                };
                if !cam_meta.file_type().is_dir() {
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
                    let Ok(meta) = std::fs::symlink_metadata(&file_path) else {
                        continue;
                    };
                    if !meta.file_type().is_file() {
                        continue;
                    }
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
                    let dbfs = probe_mp4_audio_dbfs_with_policy(&file_path, false);
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

/// Builds the hardened FFmpeg CLI arguments for segment recording (`-nostdin`,
/// `-protocol_whitelist rtsp,rtsps,rtp,tcp,udp,tls,crypto`), injecting `-itsoffset <sec>` and
/// stream mapping when `audio_offset_ms != 0` to compensate for RTSP audio/video lip-sync drift.
/// Rejects URLs starting with `-`, containing control/whitespace characters, or using disallowed schemes.
pub fn build_recorder_ffmpeg_args(
    rtsp_url: &str,
    out_pattern: &str,
    audio_offset_ms: i32,
) -> Result<Vec<String>> {
    let trimmed_url = rtsp_url.trim();
    if rtsp_url.starts_with('-')
        || trimmed_url.starts_with('-')
        || !ninjanvr_config::is_safe_camera_input_url(trimmed_url)
    {
        anyhow::bail!("disallowed recorder RTSP URL");
    }
    let Some((scheme, _)) = trimmed_url.split_once("://") else {
        anyhow::bail!("recorder URL missing scheme");
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "rtsp" | "rtsps") {
        anyhow::bail!("disallowed recorder URL scheme '{scheme}' (expected rtsp or rtsps)");
    }
    if out_pattern.trim().is_empty() || out_pattern.chars().any(char::is_control) {
        anyhow::bail!("invalid recorder output segment pattern");
    }

    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-nostdin".to_string(),
        "-probesize".to_string(),
        "32768".to_string(),
        "-analyzeduration".to_string(),
        "500000".to_string(),
        "-threads".to_string(),
        "1".to_string(),
        "-protocol_whitelist".to_string(),
        "rtsp,rtsps,rtp,tcp,udp,tls,crypto".to_string(),
        "-rtsp_transport".to_string(),
        "tcp".to_string(),
        "-timeout".to_string(),
        "10000000".to_string(),
        "-i".to_string(),
        trimmed_url.to_string(),
    ];

    if audio_offset_ms != 0 {
        let offset_sec = (audio_offset_ms as f64) / 1000.0;
        args.extend([
            "-protocol_whitelist".to_string(),
            "rtsp,rtsps,rtp,tcp,udp,tls,crypto".to_string(),
            "-rtsp_transport".to_string(),
            "tcp".to_string(),
            "-timeout".to_string(),
            "10000000".to_string(),
            "-itsoffset".to_string(),
            format!("{offset_sec:.3}"),
            "-i".to_string(),
            trimmed_url.to_string(),
            "-map".to_string(),
            "0:v:0".to_string(),
            "-map".to_string(),
            "1:a:0?".to_string(),
        ]);
    }

    let safe_out_pattern = sanitize_ffmpeg_path_arg(Path::new(out_pattern));
    args.extend([
        "-f".to_string(),
        "segment".to_string(),
        "-segment_time".to_string(),
        "10".to_string(),
        "-segment_format".to_string(),
        "mp4".to_string(),
        "-reset_timestamps".to_string(),
        "1".to_string(),
        "-segment_list".to_string(),
        "pipe:1".to_string(),
        "-segment_list_type".to_string(),
        "csv".to_string(),
        "-c:v".to_string(),
        "copy".to_string(),
        "-c:a".to_string(),
        "aac".to_string(),
        safe_out_pattern,
    ]);
    Ok(args)
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
    let safe_src = sanitize_ffmpeg_path_arg(src_path);
    let safe_dst = sanitize_ffmpeg_path_arg(dst_path);
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
                safe_src,
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
                safe_src,
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
                safe_src,
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
                safe_src,
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
        safe_dst,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraRecordPolicy {
    pub keep_continuous: bool,
    pub keep_motion: bool,
    pub continuous_substream: bool,
    pub retain_mode: String,
    pub audio_offset_ms: i32,
    pub audio_probe_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CameraRecorderSpec {
    rtsp_url: String,
    policy: CameraRecordPolicy,
}

struct SupervisedRecorderHandle {
    spec: CameraRecorderSpec,
    stop_flag: Arc<AtomicBool>,
    stop_notify: Arc<tokio::sync::Notify>,
    active_pid: Arc<std::sync::atomic::AtomicU32>,
    join_handle: tokio::task::JoinHandle<()>,
}

/// Computes the capped exponential reconnect backoff (`5s -> 10s -> 20s -> 40s -> 60s`)
/// with deterministic per-camera jitter (`0..500ms`).
pub fn compute_recorder_backoff_delay(
    consecutive_failures: u32,
    camera: &str,
) -> std::time::Duration {
    let shift = consecutive_failures.min(4);
    let base_secs = (5u64 << shift).min(60);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in camera.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^= (consecutive_failures as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let jitter_ms = hash % 500;
    std::time::Duration::from_millis(base_secs * 1_000 + jitter_ms)
}

/// Manages direct segment recording and retention cleanup for all configured cameras.
pub type AudioTelemetrySink = Arc<dyn Fn(&str, i64, bool) + Send + Sync>;

#[derive(Clone)]
pub struct RecordingEngine {
    config: Arc<RwLock<NinjaConfig>>,
    db: NinjaDatabase,
    media_dir: PathBuf,
    ffmpeg_bin: PathBuf,
    go2rtc_relay_active: Arc<AtomicBool>,
    activity_ledger: SharedActivityLedger,
    audio_telemetry_sink: Option<AudioTelemetrySink>,
    storage_degraded: Arc<AtomicBool>,
    storage_error: Arc<std::sync::Mutex<Option<String>>>,
    recorders: Arc<tokio::sync::Mutex<HashMap<String, SupervisedRecorderHandle>>>,
    reconcile_lock: Arc<tokio::sync::Mutex<()>>,
    bg_tasks: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    shutdown_flag: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
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
        Self::with_storage_state(
            config,
            db,
            media_dir,
            activity_ledger,
            Arc::new(AtomicBool::new(degraded)),
            Arc::new(std::sync::Mutex::new(err_msg)),
        )
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
            ffmpeg_bin: resolve_ffmpeg_bin(),
            go2rtc_relay_active: Arc::new(AtomicBool::new(false)),
            activity_ledger,
            audio_telemetry_sink: None,
            storage_degraded,
            storage_error,
            recorders: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            reconcile_lock: Arc::new(tokio::sync::Mutex::new(())),
            bg_tasks: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Overrides the `ffmpeg` executable path for isolated integration/unit testing.
    pub fn with_ffmpeg_bin(mut self, ffmpeg_bin: PathBuf) -> Self {
        self.ffmpeg_bin = ffmpeg_bin;
        self
    }

    /// Marks whether a local supervised `go2rtc` RTSP relay (`127.0.0.1:8554`) is active so
    /// credentialed camera RTSP inputs automatically route through loopback instead of CLI `argv`.
    pub fn with_go2rtc_relay(self, active: bool) -> Self {
        self.go2rtc_relay_active.store(active, Ordering::Relaxed);
        self
    }

    /// Registers an optional real-time callback invoked with `(camera, dbfs, acoustic_active)`
    /// whenever a recorded segment closes so MQTT/WS and `/api/stats` reflect measured audio levels.
    pub fn with_audio_telemetry_sink(mut self, sink: AudioTelemetrySink) -> Self {
        self.audio_telemetry_sink = Some(sink);
        self
    }

    /// Returns the sorted list of cameras with active supervised recorder loops.
    pub async fn active_recorder_cameras(&self) -> Vec<String> {
        let map = self.recorders.lock().await;
        let mut names: Vec<String> = map
            .iter()
            .filter(|(_, h)| !h.join_handle.is_finished())
            .map(|(k, _)| k.clone())
            .collect();
        names.sort();
        names
    }

    fn desired_camera_specs(
        cfg: &NinjaConfig,
        go2rtc_relay_active: bool,
    ) -> HashMap<String, CameraRecorderSpec> {
        cfg.cameras
            .iter()
            .filter(|(name, c)| c.enabled && c.record.enabled && is_safe_identifier(name))
            .filter_map(|(name, c)| {
                let use_sub_default =
                    c.record.continuous_substream || cfg.record.continuous_substream;
                let (role_used, raw_input_url) =
                    if use_sub_default && c.record.continuous.days == 0.0 {
                        if let Some(det) = cfg.camera_input_for_role(name, "detect") {
                            ("detect", det)
                        } else {
                            ("record", cfg.camera_input_for_role(name, "record")?)
                        }
                    } else {
                        ("record", cfg.camera_input_for_role(name, "record")?)
                    };
                let input_url = cfg.prefer_local_go2rtc_relay_url(
                    name,
                    role_used,
                    &raw_input_url,
                    go2rtc_relay_active,
                );
                if !ninjanvr_config::is_safe_camera_input_url(&input_url) {
                    warn!("Skipping recorder for camera {name}: unsafe stream input URL");
                    return None;
                }
                let has_pre_post_buffer = c.record.alerts.pre_capture > 0
                    || c.record.alerts.post_capture > 0
                    || c.record.detections.pre_capture > 0
                    || c.record.detections.post_capture > 0;
                let policy = CameraRecordPolicy {
                    keep_continuous: (c.record.continuous.days.is_finite()
                        && c.record.continuous.days > 0.0)
                        || use_sub_default
                        || has_pre_post_buffer,
                    keep_motion: c.record.motion.days.is_finite() && c.record.motion.days > 0.0,
                    continuous_substream: use_sub_default,
                    retain_mode: c.record.alerts.retain.mode.clone(),
                    audio_offset_ms: c.ffmpeg.audio_offset_ms.unwrap_or(0),
                    audio_probe_enabled: c.audio.enabled || cfg.audio.enabled,
                };
                Some((
                    name.clone(),
                    CameraRecorderSpec {
                        rtsp_url: input_url,
                        policy,
                    },
                ))
            })
            .collect()
    }

    fn signal_recorder_sigterm(pid: u32) {
        #[cfg(unix)]
        if let Ok(raw_pid) = i32::try_from(pid)
            && raw_pid > 1
        {
            let target = nix::unistd::Pid::from_raw(raw_pid);
            if nix::sys::signal::killpg(target, nix::sys::signal::Signal::SIGTERM).is_err() {
                let _ = nix::sys::signal::kill(target, nix::sys::signal::Signal::SIGTERM);
            }
        }
    }

    async fn stop_handles_gracefully(&self, mut handles: Vec<(String, SupervisedRecorderHandle)>) {
        if handles.is_empty() {
            return;
        }
        for (_, h) in &handles {
            h.stop_flag.store(true, Ordering::SeqCst);
            let pid = h.active_pid.load(Ordering::SeqCst);
            if pid > 0 {
                Self::signal_recorder_sigterm(pid);
            }
            h.stop_notify.notify_waiters();
        }

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(4);
        for (_, h) in &mut handles {
            let now = tokio::time::Instant::now();
            if now < deadline {
                if tokio::time::timeout(deadline - now, &mut h.join_handle)
                    .await
                    .is_err()
                {
                    h.join_handle.abort();
                }
            } else {
                h.join_handle.abort();
            }
        }

        for (camera, h) in handles {
            let staging_dir = self
                .media_dir
                .join("recordings")
                .join(".staging")
                .join(&camera);
            self.sweep_staging_dir(&camera, &staging_dir, &h.spec.policy)
                .await;
        }
    }

    /// Reconciles active supervised per-camera FFmpeg recorders against `self.config`:
    /// stops removed/disabled cameras or cameras whose stream URL / `CameraRecordPolicy` changed,
    /// and spawns recorder workers for newly added or enabled cameras.
    pub async fn reconcile(&self) {
        let _reconcile_guard = self.reconcile_lock.lock().await;
        if self.shutdown_flag.load(Ordering::SeqCst) {
            return;
        }
        let desired = {
            let cfg = self.config.read().await;
            Self::desired_camera_specs(&cfg, self.go2rtc_relay_active.load(Ordering::Relaxed))
        };

        let to_stop = {
            let mut map = self.recorders.lock().await;
            let stale_keys: Vec<String> = map
                .iter()
                .filter_map(|(cam, handle)| {
                    let keep = desired.get(cam).is_some_and(|spec| {
                        *spec == handle.spec && !handle.join_handle.is_finished()
                    });
                    (!keep).then(|| cam.clone())
                })
                .collect();
            let mut removed = Vec::with_capacity(stale_keys.len());
            for key in stale_keys {
                if let Some(handle) = map.remove(&key) {
                    removed.push((key, handle));
                }
            }
            removed
        };

        self.stop_handles_gracefully(to_stop).await;

        if self.shutdown_flag.load(Ordering::SeqCst) {
            return;
        }

        let mut map = self.recorders.lock().await;
        if self.shutdown_flag.load(Ordering::SeqCst) {
            return;
        }
        for (camera_name, spec) in desired {
            if map
                .get(&camera_name)
                .is_some_and(|h| !h.join_handle.is_finished() && h.spec == spec)
            {
                continue;
            }
            let stop_flag = Arc::new(AtomicBool::new(false));
            let stop_notify = Arc::new(tokio::sync::Notify::new());
            let active_pid = Arc::new(std::sync::atomic::AtomicU32::new(0));

            let engine = self.clone();
            let cam_task = camera_name.clone();
            let url_task = spec.rtsp_url.clone();
            let policy_task = spec.policy.clone();
            let stop_flag_task = Arc::clone(&stop_flag);
            let stop_notify_task = Arc::clone(&stop_notify);
            let pid_task = Arc::clone(&active_pid);

            let join_handle = tokio::spawn(async move {
                engine
                    .run_camera_recorder_loop(
                        cam_task,
                        url_task,
                        policy_task,
                        stop_flag_task,
                        stop_notify_task,
                        pid_task,
                    )
                    .await;
            });

            if let Some(prev) = map.insert(
                camera_name,
                SupervisedRecorderHandle {
                    spec,
                    stop_flag,
                    stop_notify,
                    active_pid,
                    join_handle,
                },
            ) {
                prev.stop_flag.store(true, Ordering::SeqCst);
                prev.stop_notify.notify_waiters();
                prev.join_handle.abort();
            }
        }
    }

    /// Gracefully stops all supervised camera recorders (`SIGTERM` -> wait up to `4s` for trailing
    /// `moov` atom flush -> final `sweep_staging_dir` commit to SQLite) and halts background workers.
    pub async fn shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        self.shutdown_notify.notify_waiters();

        let drained: Vec<(String, SupervisedRecorderHandle)> = {
            let mut map = self.recorders.lock().await;
            map.drain().collect()
        };
        self.stop_handles_gracefully(drained).await;

        let bg: Vec<tokio::task::JoinHandle<()>> = {
            let mut tasks = self.bg_tasks.lock().await;
            std::mem::take(&mut *tasks)
        };
        for h in bg {
            h.abort();
        }
    }

    pub async fn start(self: Arc<Self>) {
        self.shutdown_flag.store(false, Ordering::SeqCst);
        self.reconcile().await;

        let mut bg = self.bg_tasks.lock().await;
        let cleanup_engine = (*self).clone();
        bg.push(tokio::spawn(async move {
            cleanup_engine.run_retention_cleanup_loop().await;
        }));

        let preview_engine = (*self).clone();
        bg.push(tokio::spawn(async move {
            preview_engine.run_preview_generator_loop().await;
        }));

        let recovery_engine = (*self).clone();
        bg.push(tokio::spawn(async move {
            recovery_engine.run_storage_recovery_probe_loop().await;
        }));

        let dbfs_repair_engine = (*self).clone();
        bg.push(tokio::spawn(async move {
            dbfs_repair_engine.run_legacy_dbfs_repair_once().await;
        }));
    }

    async fn run_legacy_dbfs_repair_once(&self) {
        let (configured_cold_dir, any_audio_enabled) = {
            let cfg = self.config.read().await;
            let audio_on = cfg.audio.enabled || cfg.cameras.values().any(|c| c.audio.enabled);
            (cfg.record.storage_tiers.cold_dir.clone(), audio_on)
        };
        loop {
            if self.shutdown_flag.load(Ordering::SeqCst) {
                break;
            }
            let engine_ref = self.clone();
            let cold_ref = configured_cold_dir.clone();
            let repaired_batch = tokio::task::spawn_blocking(move || {
                let batch = engine_ref
                    .db
                    .list_recordings_by_dbfs(-5, 100)
                    .unwrap_or_default();
                if batch.is_empty() {
                    return 0usize;
                }
                let count = batch.len();
                for (id, db_path) in batch {
                    let real_dbfs = if let Some(media_path) =
                        engine_ref.resolve_contained_recording_file(&db_path, cold_ref.as_deref())
                    {
                        let d = probe_mp4_audio_dbfs_with_bin(
                            &media_path,
                            any_audio_enabled,
                            Some(&engine_ref.ffmpeg_bin),
                        );
                        if d == -5 { 0 } else { d }
                    } else {
                        0
                    };
                    let _ = engine_ref.db.update_recording_dbfs(&id, real_dbfs);
                }
                count
            })
            .await
            .unwrap_or(0);
            if repaired_batch == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    }

    async fn run_storage_recovery_probe_loop(&self) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                _ = self.shutdown_notify.notified() => break,
            }
            if self.shutdown_flag.load(Ordering::SeqCst) {
                break;
            }
            let media_dir = self.media_dir.clone();
            let probe_res = tokio::task::spawn_blocking(move || probe_storage_writable(&media_dir))
                .await
                .unwrap_or_else(|e| Err(format!("storage probe task panicked: {e}")));
            match probe_res {
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
        let staging_dir_buf = staging_dir.to_path_buf();
        let valid_mp4s = tokio::task::spawn_blocking(move || {
            let mut valid = Vec::new();
            let Ok(entries) = std::fs::read_dir(&staging_dir_buf) else {
                return valid;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("mp4") {
                    if probe_mp4_duration_fast(&path).is_some() {
                        valid.push(path);
                    } else {
                        let _ = std::fs::remove_file(&path);
                    }
                } else {
                    let _ = std::fs::remove_file(&path);
                }
            }
            valid.sort();
            valid
        })
        .await
        .unwrap_or_default();

        for path in valid_mp4s {
            let _ = self
                .finalize_closed_segment(camera, &path, 10.0, policy)
                .await;
        }
    }

    async fn run_camera_recorder_loop(
        &self,
        camera: String,
        rtsp_url: String,
        policy: CameraRecordPolicy,
        stop_flag: Arc<AtomicBool>,
        stop_notify: Arc<tokio::sync::Notify>,
        active_pid: Arc<std::sync::atomic::AtomicU32>,
    ) {
        if !is_safe_identifier(&camera) {
            warn!("Refusing to start recorder for unsafe camera identifier: {camera}");
            return;
        }
        let staging_dir = self
            .media_dir
            .join("recordings")
            .join(".staging")
            .join(&camera);
        let ffmpeg_bin = self.ffmpeg_bin.clone();
        let mut consecutive_failures: u32 = 0;

        loop {
            if stop_flag.load(Ordering::SeqCst) || self.shutdown_flag.load(Ordering::SeqCst) {
                break;
            }
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
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                    _ = stop_notify.notified() => break,
                    _ = self.shutdown_notify.notified() => break,
                }
                continue;
            }
            self.sweep_staging_dir(&camera, &staging_dir, &policy).await;

            let session_start_epoch = unix_now_f64();
            let session_epoch_ms = (session_start_epoch * 1_000.0).round() as u64;
            let out_pattern = staging_dir.join(format!("{camera}_{session_epoch_ms}_seg_%09d.mp4"));
            let ff_args = match build_recorder_ffmpeg_args(
                &rtsp_url,
                out_pattern.to_string_lossy().as_ref(),
                policy.audio_offset_ms,
            ) {
                Ok(args) => args,
                Err(err) => {
                    warn!("Refusing to spawn recorder for camera {camera}: {err:#}");
                    break;
                }
            };

            let mut cmd = Command::new(&ffmpeg_bin);
            #[cfg(unix)]
            cmd.process_group(0);
            cmd.env("TZ", "UTC")
                .args(&ff_args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);

            let Ok(mut child) = cmd.spawn() else {
                warn!(
                    "ffmpeg binary ({}) not available for camera {camera} recorder; retrying",
                    ffmpeg_bin.display()
                );
                let delay = compute_recorder_backoff_delay(consecutive_failures, &camera);
                consecutive_failures = consecutive_failures.saturating_add(1);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = stop_notify.notified() => break,
                    _ = self.shutdown_notify.notified() => break,
                }
                continue;
            };

            let session_started = tokio::time::Instant::now();
            let pid = child.id().unwrap_or(0);
            active_pid.store(pid, Ordering::SeqCst);
            if stop_flag.load(Ordering::SeqCst) || self.shutdown_flag.load(Ordering::SeqCst) {
                if pid > 0 {
                    Self::signal_recorder_sigterm(pid);
                }
                let _ = tokio::time::timeout(std::time::Duration::from_millis(3500), child.wait())
                    .await;
                active_pid.store(0, Ordering::SeqCst);
                break;
            }

            info!("Started direct segment recorder for camera {camera} (PID: {pid})");
            let mut stopping = false;
            if let Some(stdout) = child.stdout.take() {
                let mut reader = BufReader::new(stdout).lines();
                loop {
                    let line_opt = if stopping {
                        match tokio::time::timeout(
                            std::time::Duration::from_millis(3500),
                            reader.next_line(),
                        )
                        .await
                        {
                            Ok(Ok(opt)) => opt,
                            _ => break,
                        }
                    } else {
                        tokio::select! {
                            res = reader.next_line() => res.ok().flatten(),
                            _ = stop_notify.notified() => {
                                stopping = true;
                                if pid > 0 {
                                    Self::signal_recorder_sigterm(pid);
                                }
                                continue;
                            }
                            _ = self.shutdown_notify.notified() => {
                                stopping = true;
                                if pid > 0 {
                                    Self::signal_recorder_sigterm(pid);
                                }
                                continue;
                            }
                        }
                    };
                    let Some(line) = line_opt else {
                        break;
                    };
                    let parts: Vec<&str> = line.trim().split(',').collect();
                    if parts.len() >= 3 {
                        let seg_filename = parts[0];
                        if !is_safe_identifier(seg_filename) {
                            warn!(
                                "Ignoring unsafe staged segment filename for {camera}: {seg_filename}"
                            );
                            continue;
                        }
                        let csv_start_off = parts[1]
                            .parse::<f64>()
                            .ok()
                            .filter(|v| v.is_finite() && *v >= 0.0);
                        let csv_end_off = parts[2].parse::<f64>().ok().filter(|v| v.is_finite());
                        let seg_dur = match (csv_start_off, csv_end_off) {
                            (Some(s), Some(e)) if e > s => e - s,
                            _ => 10.0,
                        };
                        let explicit_start = csv_start_off.map(|off| session_start_epoch + off);
                        let staged_path = staging_dir.join(seg_filename);
                        if let Err(err) = self
                            .finalize_closed_segment_at(
                                &camera,
                                &staged_path,
                                explicit_start,
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

            let status = if stopping
                || stop_flag.load(Ordering::SeqCst)
                || self.shutdown_flag.load(Ordering::SeqCst)
            {
                if pid > 0 {
                    Self::signal_recorder_sigterm(pid);
                }
                match tokio::time::timeout(std::time::Duration::from_millis(3500), child.wait())
                    .await
                {
                    Ok(res) => res,
                    Err(_) => {
                        let _ = child.kill().await;
                        child.wait().await
                    }
                }
            } else {
                child.wait().await
            };
            active_pid.store(0, Ordering::SeqCst);

            if stopping
                || stop_flag.load(Ordering::SeqCst)
                || self.shutdown_flag.load(Ordering::SeqCst)
            {
                break;
            }

            if session_started.elapsed() >= std::time::Duration::from_secs(30) {
                consecutive_failures = 0;
            }
            let delay = compute_recorder_backoff_delay(consecutive_failures, &camera);
            consecutive_failures = consecutive_failures.saturating_add(1);
            warn!(
                "Recorder for {camera} exited ({status:?}); reconnecting in {:.1}s",
                delay.as_secs_f64()
            );
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = stop_notify.notified() => break,
                _ = self.shutdown_notify.notified() => break,
            }
        }
    }

    async fn finalize_closed_segment(
        &self,
        camera: &str,
        staged_path: &Path,
        csv_duration: f64,
        policy: &CameraRecordPolicy,
    ) -> Result<()> {
        self.finalize_closed_segment_at(camera, staged_path, None, csv_duration, policy)
            .await
    }

    async fn finalize_closed_segment_at(
        &self,
        camera: &str,
        staged_path: &Path,
        explicit_start_time: Option<f64>,
        csv_duration: f64,
        policy: &CameraRecordPolicy,
    ) -> Result<()> {
        if !is_safe_identifier(camera) {
            anyhow::bail!("unsafe camera identifier: {camera}");
        }
        let record_allowed = {
            let ledger = self.activity_ledger.read().await;
            ledger.get(camera).is_none_or(|r| r.is_record_enabled())
        };

        struct StagedSegmentProbe {
            size_bytes: u64,
            duration: f64,
            start_time: f64,
            end_time: f64,
            probed_dbfs: i64,
        }

        let prel_ledger_dbfs =
            if let Some(exp_start) = explicit_start_time.filter(|ts| ts.is_finite() && *ts > 0.0) {
                let prel_end = exp_start + csv_duration.clamp(0.5, 60.0);
                let ledger = self.activity_ledger.read().await;
                ledger.get(camera).map_or(0, |ring| {
                    ring.aggregate_range_with_audio(exp_start, prel_end).3
                })
            } else {
                0
            };

        let staged_probe_path = staged_path.to_path_buf();
        let cam_probe = camera.to_string();
        let audio_probe_enabled = policy.audio_probe_enabled && prel_ledger_dbfs == 0;
        let ffmpeg_bin_probe = self.ffmpeg_bin.clone();
        let probe_res =
            tokio::task::spawn_blocking(move || -> Result<Option<StagedSegmentProbe>> {
                let meta = match std::fs::symlink_metadata(&staged_probe_path) {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => {
                        return Err(e).with_context(|| {
                            format!(
                                "failed to stat staged segment {}",
                                staged_probe_path.display()
                            )
                        });
                    }
                };
                if !meta.file_type().is_file() {
                    anyhow::bail!(
                        "staged segment {} is not a regular file",
                        staged_probe_path.display()
                    );
                }
                let size_bytes = meta.len();
                if size_bytes < 1024 || !record_allowed {
                    let _ = std::fs::remove_file(&staged_probe_path);
                    return Ok(None);
                }
                let duration = probe_mp4_duration_fast(&staged_probe_path)
                    .unwrap_or(csv_duration)
                    .clamp(0.5, 60.0);
                let start_time = explicit_start_time
                    .filter(|ts| ts.is_finite() && *ts > 0.0)
                    .or_else(|| parse_staged_start_time(&cam_probe, &staged_probe_path))
                    .unwrap_or_else(|| unix_now_f64() - duration);
                let end_time = start_time + duration;
                let probed_dbfs = probe_mp4_audio_dbfs_with_bin(
                    &staged_probe_path,
                    audio_probe_enabled,
                    Some(&ffmpeg_bin_probe),
                );
                Ok(Some(StagedSegmentProbe {
                    size_bytes,
                    duration,
                    start_time,
                    end_time,
                    probed_dbfs,
                }))
            })
            .await
            .context("staged segment probe task panicked")??;

        let Some(StagedSegmentProbe {
            size_bytes,
            duration,
            start_time,
            end_time,
            probed_dbfs,
        }) = probe_res
        else {
            return Ok(());
        };

        let (motion, objects, regions, ledger_dbfs) = {
            let ledger = self.activity_ledger.read().await;
            ledger.get(camera).map_or((0, 0, 0, 0), |ring| {
                ring.aggregate_range_with_audio(start_time, end_time)
            })
        };
        let dbfs = if ledger_dbfs != 0 {
            ledger_dbfs
        } else if probed_dbfs != 0 {
            probed_dbfs
        } else {
            prel_ledger_dbfs
        };

        let should_keep = if policy.keep_continuous || policy.retain_mode == "all" {
            true
        } else if policy.keep_motion || policy.retain_mode == "motion" {
            motion > 0 || objects > 0 || (dbfs != 0 && dbfs > -45)
        } else {
            objects > 0
        };

        let audio_cfg = {
            let cfg = self.config.read().await;
            cfg.cameras.get(camera).map_or_else(
                || cfg.audio.clone(),
                |c| {
                    if c.audio.enabled {
                        c.audio.clone()
                    } else {
                        cfg.audio.clone()
                    }
                },
            )
        };
        let acoustic_trigger = check_acoustic_event_trigger(dbfs, &audio_cfg);
        if let Some(ref sink) = self.audio_telemetry_sink {
            sink(camera, dbfs, acoustic_trigger.is_some());
        }

        let db_ref = self.db.clone();
        let media_dir_ref = self.media_dir.clone();
        let staged_commit_path = staged_path.to_path_buf();
        let cam_commit = camera.to_string();
        let continuous_substream = policy.continuous_substream;

        tokio::task::spawn_blocking(move || -> Result<()> {
            if !should_keep {
                let _ = std::fs::remove_file(&staged_commit_path);
                return Ok(());
            }

            let dt = Utc
                .timestamp_opt(start_time as i64, 0)
                .single()
                .unwrap_or_else(Utc::now);
            let date_dir = dt.format("%Y-%m-%d").to_string();
            let hour_dir = dt.format("%H").to_string();
            let mut file_name = format!("{}.mp4", dt.format("%M.%S"));

            let target_dir = media_dir_ref
                .join("recordings")
                .join(&date_dir)
                .join(&hour_dir)
                .join(&cam_commit);
            std::fs::create_dir_all(&target_dir)?;

            let mut final_disk_path = target_dir.join(&file_name);
            if final_disk_path.exists() {
                file_name = format!("{}-{}.mp4", dt.format("%M.%S"), &random_suffix()[..4]);
                final_disk_path = target_dir.join(&file_name);
            }

            std::fs::rename(&staged_commit_path, &final_disk_path).or_else(|_| {
                std::fs::copy(&staged_commit_path, &final_disk_path)?;
                std::fs::remove_file(&staged_commit_path)
            })?;

            let db_canonical_path =
                format!("/media/ninjanvr/recordings/{date_dir}/{hour_dir}/{cam_commit}/{file_name}");
            let seg_size_mb = ((size_bytes as f64) / (1024.0 * 1024.0) * 100.0).round() / 100.0;
            let id = format!("{:.1}-{}", start_time, random_suffix());

            db_ref.insert_recording(&RecordingRow {
                id: id.clone(),
                camera: cam_commit.clone(),
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
            if let Some(audio_label) = acoustic_trigger {
                let ev_id = format!("audio-{id}");
                if let Err(err) = db_ref.upsert_event(&EventDto {
                    id: ev_id.clone(),
                    camera: cam_commit.clone(),
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
                }) {
                    warn!("Failed to persist acoustic event {ev_id} for {cam_commit}: {err:#}");
                } else {
                    if let Err(err) = db_ref.insert_review_segment(&ReviewItemDto {
                        id: format!("rev-{ev_id}"),
                        camera: cam_commit.clone(),
                        start_time,
                        end_time: Some(end_time),
                        severity: "detection".to_string(),
                        thumb_path: format!("/api/{cam_commit}/latest.jpg"),
                        data: json!({
                            "detections": [ev_id],
                            "objects": ["audio"],
                            "zones": [],
                            "audio": [audio_label],
                            "dbfs": dbfs,
                            "thumb_time": start_time
                        }),
                        has_been_reviewed: false,
                    }) {
                        warn!(
                            "Failed to persist acoustic review segment rev-{ev_id} for {cam_commit}: {err:#}"
                        );
                    }
                    if let Err(err) = db_ref.insert_timeline_entry(
                        start_time,
                        &cam_commit,
                        "audio",
                        Some(&ev_id),
                        "heard",
                        &json!({
                            "label": audio_label,
                            "dbfs": dbfs,
                            "score": 0.90,
                            "top_score": 0.90
                        }),
                    ) {
                        warn!(
                            "Failed to persist acoustic timeline entry for {ev_id} on {cam_commit}: {err:#}"
                        );
                    }
                }
            }

            debug!(
                "Recorded segment for {cam_commit}: {:.2}s ({:.2} MB, motion={}, objects={}, dbfs={})",
                duration, seg_size_mb, motion, objects, dbfs
            );
            let tier = if continuous_substream && objects == 0 && motion == 0 {
                "sub"
            } else {
                "main"
            };
            if let Err(err) = db_ref.set_recording_stream_tier(&id, tier) {
                warn!("Failed to persist recording stream tier for {id} on {cam_commit}: {err:#}");
            }
            Ok(())
        })
        .await
        .context("staged segment finalize task panicked")?
    }

    async fn run_retention_cleanup_loop(&self) {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
            _ = self.shutdown_notify.notified() => return,
        }
        loop {
            if self.shutdown_flag.load(Ordering::SeqCst) {
                break;
            }
            let expire_interval = self.run_retention_pass_once(unix_now_f64()).await;
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(expire_interval)) => {}
                _ = self.shutdown_notify.notified() => break,
            }
        }
    }

    /// Executes a single deterministic retention, quota-eviction, cold-compaction, tier-migration,
    /// and expired share-artifact cleanup pass at `now` (Unix epoch seconds), returning the
    /// configured `expire_interval` in seconds.
    pub async fn run_retention_pass_once(&self, now: f64) -> u64 {
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
            preview_disabled: bool,
        }
        {
            let (expire_interval, camera_retentions, global_quota_gb, global_min_free_gb): (
                u64,
                Vec<CameraRetentionEntry>,
                Option<f64>,
                Option<f64>,
            ) = {
                let cfg = self.config.read().await;
                let interval = cfg.record.expire_interval.max(30);
                let global_quota = sanitize_quota_gb(cfg.record.max_size_gb)
                    .or_else(|| sanitize_quota_gb(cfg.record.alerts.retain.max_size_gb))
                    .or_else(|| sanitize_quota_gb(cfg.record.detections.retain.max_size_gb));
                let global_min_free = sanitize_quota_gb(cfg.record.min_free_gb)
                    .or_else(|| sanitize_quota_gb(cfg.record.alerts.retain.min_free_gb))
                    .or_else(|| sanitize_quota_gb(cfg.record.detections.retain.min_free_gb));
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
                        let continuous_days =
                            sanitize_retention_days(c.record.continuous.days, 7.0, min_buffer_days);
                        let alerts_days =
                            sanitize_retention_days(c.record.alerts.retain.days, 7.0, 0.1);
                        let detections_days =
                            sanitize_retention_days(c.record.detections.retain.days, 7.0, 0.1);
                        let event_days = alerts_days.min(detections_days);
                        let motion_days =
                            if c.record.motion.days.is_finite() && c.record.motion.days > 0.0 {
                                sanitize_retention_days(
                                    c.record.motion.days,
                                    continuous_days,
                                    continuous_days,
                                )
                            } else if c.record.alerts.retain.mode != "active_objects"
                                || c.record.detections.retain.mode != "active_objects"
                            {
                                event_days.max(continuous_days)
                            } else {
                                continuous_days
                            };
                        let quota_gb = sanitize_quota_gb(c.record.max_size_gb)
                            .or_else(|| sanitize_quota_gb(c.record.alerts.retain.max_size_gb))
                            .or_else(|| sanitize_quota_gb(c.record.detections.retain.max_size_gb));
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
                        let preview_disabled =
                            ninjanvr_config::is_preview_disabled(&c.record.preview.quality);
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
                            preview_disabled,
                        }
                    })
                    .collect();
                (interval, cams, global_quota, global_min_free)
            };

            let tier_cfg = {
                let cfg = self.config.read().await;
                cfg.record.storage_tiers.clone()
            };
            let configured_cold_dir = tier_cfg.cold_dir.clone();

            struct DeferredCameraCompaction {
                camera: String,
                compact_after_hours: u32,
                compact_crf: u8,
                compact_idle_only: bool,
                pre_capture_sec: f64,
                post_capture_sec: f64,
                compact_codec: String,
                compact_preset: String,
                compact_hwaccel: String,
            }
            let mut pending_compactions: Vec<DeferredCameraCompaction> = Vec::new();
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
                preview_disabled,
            } in camera_retentions
            {
                if mode_tiering {
                    let idle_main_cutoff = now - (continuous_days.min(1.0) * 86400.0);
                    let engine_idle = self.clone();
                    let cam_idle = camera.clone();
                    let cold_idle = configured_cold_dir.clone();
                    let rec_root_idle = recordings_root.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(idle_paths) = engine_idle
                            .db
                            .delete_expired_idle_main_recordings(&cam_idle, idle_main_cutoff)
                        {
                            for db_path in &idle_paths {
                                let _ = engine_idle.remove_contained_recording_file(
                                    db_path,
                                    cold_idle.as_deref(),
                                    &rec_root_idle,
                                );
                            }
                        }
                    })
                    .await;
                }
                let continuous_cutoff = now - continuous_days * 86400.0;
                let motion_cutoff = now - motion_days * 86400.0;
                let alerts_cutoff = now - alerts_days * 86400.0;
                let detections_cutoff = now - detections_days * 86400.0;
                let event_recording_cutoff = alerts_cutoff.min(detections_cutoff);
                loop {
                    let engine_exp = self.clone();
                    let cam_ref = camera.clone();
                    let cold_exp = configured_cold_dir.clone();
                    let rec_root_exp = recordings_root.clone();
                    let Ok(Ok((batch_len, removed_files))) =
                        tokio::task::spawn_blocking(move || -> Result<(usize, usize)> {
                            let expired_paths =
                                engine_exp.db.delete_expired_recordings_with_window(
                                    &cam_ref,
                                    continuous_cutoff,
                                    motion_cutoff,
                                    event_recording_cutoff,
                                    pre_capture_sec,
                                    post_capture_sec,
                                )?;
                            if expired_paths.is_empty() {
                                return Ok((0, 0));
                            }
                            let batch_len = expired_paths.len();
                            let mut removed_files = 0usize;
                            for db_path in &expired_paths {
                                if engine_exp.remove_contained_recording_file(
                                    db_path,
                                    cold_exp.as_deref(),
                                    &rec_root_exp,
                                ) {
                                    removed_files += 1;
                                }
                            }
                            Ok((batch_len, removed_files))
                        })
                        .await
                    else {
                        break;
                    };
                    if batch_len == 0 {
                        break;
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
                    && max_gb.is_finite()
                    && max_gb > 0.0
                    && (max_gb * 1024.0).is_finite()
                {
                    let engine_quota = self.clone();
                    let cam_quota = camera.clone();
                    let cold_quota = configured_cold_dir.clone();
                    let rec_root_quota = recordings_root.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(quota_paths) = engine_quota
                            .db
                            .delete_recordings_over_quota_mb(&cam_quota, max_gb * 1024.0)
                        {
                            for db_path in &quota_paths {
                                let _ = engine_quota.remove_contained_recording_file(
                                    db_path,
                                    cold_quota.as_deref(),
                                    &rec_root_quota,
                                );
                            }
                        }
                    })
                    .await;
                }

                let engine_ev = self.clone();
                let cam_ev = camera.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok((_ev_del, _rev_del, clip_paths)) = engine_ev
                        .db
                        .cleanup_camera_expired_events_and_reviews_in_dir(
                            &engine_ev.media_dir,
                            &cam_ev,
                            alerts_cutoff,
                            detections_cutoff,
                        )
                    {
                        for cp in clip_paths {
                            let _ = engine_ev.remove_contained_media_file(&cp);
                        }
                    }
                    if preview_disabled {
                        engine_ev.purge_disabled_camera_previews(&cam_ev);
                    }
                })
                .await;

                if compact_enabled {
                    pending_compactions.push(DeferredCameraCompaction {
                        camera,
                        compact_after_hours,
                        compact_crf,
                        compact_idle_only,
                        pre_capture_sec,
                        post_capture_sec,
                        compact_codec,
                        compact_preset,
                        compact_hwaccel,
                    });
                }
            }

            // Global disk quota (`max_size_gb`) and minimum free disk headroom (`min_free_gb`) eviction pass
            {
                let engine_glob = self.clone();
                let rec_root_glob = recordings_root.clone();
                let cold_glob = configured_cold_dir.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let mut effective_global_max_mb =
                        sanitize_quota_gb(global_quota_gb).map(|g| g * 1024.0);
                    if let Some(min_free_gb) = sanitize_quota_gb(global_min_free_gb)
                        && let Ok(stat) = nix::sys::statvfs::statvfs(&rec_root_glob)
                    {
                        let bsize = stat.fragment_size() as f64;
                        let free_mb = (stat.blocks_available() as f64 * bsize) / (1024.0 * 1024.0);
                        if let Ok(usage_val) = engine_glob.db.query_recordings_storage_usage()
                            && let Some(obj) = usage_val.as_object()
                        {
                            let current_rec_mb: f64 = obj
                                .values()
                                .filter_map(|v| v["usage"].as_f64())
                                .filter(|v| v.is_finite() && *v >= 0.0)
                                .sum();
                            if let Some(allowed_mb) =
                                compute_headroom_allowed_mb(current_rec_mb, free_mb, min_free_gb)
                            {
                                effective_global_max_mb = Some(
                                    effective_global_max_mb
                                        .map_or(allowed_mb, |m| m.min(allowed_mb)),
                                );
                            }
                        }
                    }
                    if let Some(max_mb) = effective_global_max_mb
                        && max_mb.is_finite()
                        && max_mb > 0.0
                        && let Ok(evicted_paths) = engine_glob
                            .db
                            .delete_global_recordings_over_quota_mb(max_mb)
                    {
                        for db_path in &evicted_paths {
                            let _ = engine_glob.remove_contained_recording_file(
                                db_path,
                                cold_glob.as_deref(),
                                &rec_root_glob,
                            );
                        }
                    }
                })
                .await;
            }

            // Cold compaction pass runs after age + quota/headroom eviction and skips `.compact.mp4` staging
            // whenever free disk space is below `min_free_gb`.
            for DeferredCameraCompaction {
                camera,
                compact_after_hours,
                compact_crf,
                compact_idle_only,
                pre_capture_sec,
                post_capture_sec,
                compact_codec,
                compact_preset,
                compact_hwaccel,
            } in pending_compactions
            {
                if let Some(min_free_gb) = sanitize_quota_gb(global_min_free_gb) {
                    let rec_root_chk = recordings_root.clone();
                    let low_headroom = tokio::task::spawn_blocking(move || {
                        nix::sys::statvfs::statvfs(&rec_root_chk).is_ok_and(|stat| {
                            let free_gb = (stat.blocks_available() as f64
                                * stat.fragment_size() as f64)
                                / (1024.0 * 1024.0 * 1024.0);
                            free_gb < min_free_gb
                        })
                    })
                    .await
                    .unwrap_or(false);
                    if low_headroom {
                        continue;
                    }
                }
                let compact_before = now - (compact_after_hours as f64) * 3600.0;
                let db_cand = self.db.clone();
                let cam_cand = camera;
                let candidates = tokio::task::spawn_blocking(move || {
                    db_cand.list_cold_compaction_candidates(
                        &cam_cand,
                        compact_before,
                        compact_idle_only,
                        pre_capture_sec,
                        post_capture_sec,
                        12,
                    )
                })
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();

                for (seg_id, db_path) in candidates {
                    let engine_prep = self.clone();
                    let cold_prep = configured_cold_dir.clone();
                    let seg_id_prep = seg_id.clone();
                    let prep_res =
                        tokio::task::spawn_blocking(move || -> Option<(PathBuf, u64)> {
                            let Some(disk_path) = engine_prep
                                .resolve_contained_recording_file(&db_path, cold_prep.as_deref())
                            else {
                                let _ =
                                    engine_prep.db.update_recording_compacted(&seg_id_prep, 0.0);
                                return None;
                            };
                            let Ok(orig_meta) = std::fs::symlink_metadata(&disk_path) else {
                                let _ =
                                    engine_prep.db.update_recording_compacted(&seg_id_prep, 0.0);
                                return None;
                            };
                            if !orig_meta.file_type().is_file() {
                                let _ =
                                    engine_prep.db.update_recording_compacted(&seg_id_prep, 0.0);
                                return None;
                            };
                            let orig_len = orig_meta.len();
                            if orig_len < 65_536 {
                                let mb =
                                    ((orig_len as f64) / (1024.0 * 1024.0) * 100.0).round() / 100.0;
                                let _ = engine_prep.db.update_recording_compacted(&seg_id_prep, mb);
                                return None;
                            }
                            Some((disk_path, orig_len))
                        })
                        .await
                        .ok()
                        .flatten();

                    let Some((disk_path, orig_len)) = prep_res else {
                        continue;
                    };

                    let compact_nonce: u64 = rand::random();
                    let tmp_out =
                        disk_path.with_extension(format!("{compact_nonce:016x}.compact.mp4"));
                    let _tmp_compact_guard = TempPathGuard(tmp_out.clone());
                    let primary_args = build_cold_compaction_ffmpeg_args(
                        &disk_path,
                        &tmp_out,
                        &compact_codec,
                        &compact_preset,
                        &compact_hwaccel,
                        compact_crf,
                    );
                    let mut cmd = Command::new(&ffmpeg_bin);
                    cmd.args(&primary_args)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .kill_on_drop(true);
                    let mut status = cmd.status().await;
                    if !status.as_ref().is_ok_and(|s| s.success()) && compact_hwaccel != "none" {
                        let fallback_args = build_cold_compaction_ffmpeg_args(
                            &disk_path,
                            &tmp_out,
                            &compact_codec,
                            &compact_preset,
                            "none",
                            compact_crf,
                        );
                        let mut fb_cmd = Command::new(&ffmpeg_bin);
                        fb_cmd
                            .args(&fallback_args)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .kill_on_drop(true);
                        status = fb_cmd.status().await;
                    }
                    let ff_success = status.is_ok_and(|s| s.success());
                    let db_fin = self.db.clone();
                    let tmp_out_fin = tmp_out.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let mut final_mb =
                            ((orig_len as f64) / (1024.0 * 1024.0) * 100.0).round() / 100.0;
                        if ff_success
                            && let Ok(new_meta) = std::fs::symlink_metadata(&tmp_out_fin)
                            && new_meta.file_type().is_file()
                            && new_meta.len() > 4096
                            && new_meta.len() < orig_len
                            && probe_mp4_duration_fast(&tmp_out_fin).is_some()
                            && std::fs::symlink_metadata(&disk_path)
                                .is_ok_and(|m| m.file_type().is_file())
                        {
                            if std::fs::rename(&tmp_out_fin, &disk_path).is_ok() {
                                final_mb = ((new_meta.len() as f64) / (1024.0 * 1024.0) * 100.0)
                                    .round()
                                    / 100.0;
                            } else {
                                let _ = std::fs::remove_file(&tmp_out_fin);
                            }
                        } else {
                            let _ = std::fs::remove_file(&tmp_out_fin);
                        }
                        let _ = db_fin.update_recording_compacted(&seg_id, final_mb);
                    })
                    .await;
                }
            }

            // Hot-to-Cold tiered storage migration pass
            if tier_cfg.enabled
                && let Some(cold_dir) = tier_cfg.cold_dir.clone()
            {
                let migrate_before = now - (tier_cfg.migrate_after_hours.max(1) as f64) * 3600.0;
                let db_tier = self.db.clone();
                let media_dir_tier = self.media_dir.clone();
                if let Ok(Err(err)) = tokio::task::spawn_blocking(move || {
                    migrate_hot_segments_to_cold(
                        &db_tier,
                        &media_dir_tier,
                        &cold_dir,
                        migrate_before,
                    )
                })
                .await
                {
                    warn!("Hot-to-cold storage tier migration skipped: {err:#}");
                }
            }

            // Prune expired share links and remove orphaned share clip/redacted artifacts
            let engine_share = self.clone();
            let _ = tokio::task::spawn_blocking(move || {
                engine_share.prune_expired_share_link_artifacts(now);
            })
            .await;

            // Periodic SQLite query-planner optimization & WAL checkpointing
            let db_maint = self.db.clone();
            let _ = tokio::task::spawn_blocking(move || db_maint.run_maintenance()).await;

            expire_interval
        }
    }

    fn resolve_contained_recording_file(
        &self,
        db_path: &str,
        cold_dir: Option<&Path>,
    ) -> Option<PathBuf> {
        if db_path.is_empty()
            || Path::new(db_path)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return None;
        }
        let disk_path = if let Some(rel) = strip_media_root(db_path) {
            self.media_dir.join(rel)
        } else {
            PathBuf::from(db_path)
        };
        if !disk_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
        {
            return None;
        }
        let lmeta = std::fs::symlink_metadata(&disk_path).ok()?;
        if !lmeta.file_type().is_file() {
            return None;
        }
        let canon_parent = std::fs::canonicalize(disk_path.parent()?).ok()?;
        let canon_file = std::fs::canonicalize(&disk_path).ok()?;
        if canon_file.parent() != Some(canon_parent.as_path()) {
            return None;
        }
        let cmeta = std::fs::symlink_metadata(&canon_file).ok()?;
        if !cmeta.file_type().is_file() {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if cmeta.dev() != lmeta.dev() || cmeta.ino() != lmeta.ino() {
                return None;
            }
        }
        let rec_root = self.media_dir.join("recordings");
        let in_hot = std::fs::canonicalize(&rec_root)
            .is_ok_and(|r| canon_parent.starts_with(&r) && canon_file.starts_with(&r));
        let in_cold = cold_dir
            .and_then(|c| std::fs::canonicalize(c).ok())
            .is_some_and(|c| canon_parent.starts_with(&c) && canon_file.starts_with(&c));
        if in_hot || in_cold {
            Some(canon_file)
        } else {
            None
        }
    }

    fn remove_contained_recording_file(
        &self,
        db_path: &str,
        cold_dir: Option<&Path>,
        recordings_root: &Path,
    ) -> bool {
        let Some(canon_file) = self.resolve_contained_recording_file(db_path, cold_dir) else {
            return false;
        };
        let Ok(cmeta) = std::fs::symlink_metadata(&canon_file) else {
            return false;
        };
        if !cmeta.file_type().is_file() {
            return false;
        }
        if std::fs::remove_file(&canon_file).is_ok() {
            if let Ok(canon_rec_root) = std::fs::canonicalize(recordings_root)
                && canon_file.starts_with(&canon_rec_root)
            {
                prune_empty_parent_dirs(&canon_file, &canon_rec_root);
            } else if let Some(Ok(canon_cold_root)) = cold_dir.map(std::fs::canonicalize)
                && canon_file.starts_with(&canon_cold_root)
            {
                prune_empty_parent_dirs(&canon_file, &canon_cold_root);
            }
            true
        } else {
            false
        }
    }

    fn remove_contained_media_file(&self, cp: &str) -> bool {
        if cp.is_empty()
            || Path::new(cp)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return false;
        }
        let disk_path = if let Some(rel) = strip_media_root(cp) {
            self.media_dir.join(rel)
        } else {
            PathBuf::from(cp)
        };
        if !is_allowed_media_artifact_extension(&disk_path) {
            return false;
        }
        let Ok(lmeta) = std::fs::symlink_metadata(&disk_path) else {
            return false;
        };
        if !lmeta.file_type().is_file() {
            return false;
        }
        let Some(parent) = disk_path.parent() else {
            return false;
        };
        let Ok(canon_parent) = std::fs::canonicalize(parent) else {
            return false;
        };
        let in_allowed_subdir = ["clips", "exports", "previews"].into_iter().any(|sub| {
            std::fs::canonicalize(self.media_dir.join(sub))
                .is_ok_and(|canon_sub| canon_parent.starts_with(&canon_sub))
        });
        if !in_allowed_subdir {
            return false;
        }
        let Ok(canon_file) = std::fs::canonicalize(&disk_path) else {
            return false;
        };
        if canon_file.parent() != Some(canon_parent.as_path())
            || !is_allowed_media_artifact_extension(&canon_file)
        {
            return false;
        }
        let Ok(cmeta) = std::fs::symlink_metadata(&canon_file) else {
            return false;
        };
        if !cmeta.file_type().is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if cmeta.dev() != lmeta.dev() || cmeta.ino() != lmeta.ino() {
                return false;
            }
        }
        std::fs::remove_file(&disk_path).is_ok()
    }

    fn purge_disabled_camera_previews(&self, camera: &str) {
        if let Ok(preview_paths) = self.db.delete_all_camera_previews(camera) {
            for cp in preview_paths {
                let _ = self.remove_contained_media_file(&cp);
            }
        }
        if !is_safe_identifier(camera) {
            return;
        }
        let clips_root = self.media_dir.join("clips");
        let previews_root = clips_root.join("previews");
        let cam_preview_dir = previews_root.join(camera);
        let Ok(dir_meta) = std::fs::symlink_metadata(&cam_preview_dir) else {
            return;
        };
        if !dir_meta.file_type().is_dir() {
            return;
        }
        let (Ok(canon_clips), Ok(canon_cam_dir)) = (
            std::fs::canonicalize(&clips_root),
            std::fs::canonicalize(&cam_preview_dir),
        ) else {
            return;
        };
        if !canon_cam_dir.starts_with(&canon_clips) {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(&cam_preview_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(emeta) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if !emeta.file_type().is_file() {
                    continue;
                }
                let fname = entry.file_name().to_string_lossy().into_owned();
                if fname.starts_with('.')
                    && (fname.ends_with("_concat.txt") || fname.ends_with(".tmp.mp4"))
                {
                    if let Ok(canon_entry) = std::fs::canonicalize(&path)
                        && canon_entry.parent() == Some(canon_cam_dir.as_path())
                    {
                        let _ = std::fs::remove_file(&path);
                    }
                } else {
                    let _ = self.remove_contained_media_file(&path.to_string_lossy());
                }
            }
        }
        let _ = std::fs::remove_dir(&cam_preview_dir);
    }

    fn prune_expired_share_link_artifacts(&self, now: f64) {
        let expired_or_revoked_targets =
            match self.db.list_expired_or_revoked_share_link_targets(now) {
                Ok(targets) => targets,
                Err(err) => {
                    warn!("Failed to query expired or revoked share link targets: {err:#}");
                    Vec::new()
                }
            };
        if let Err(err) = self.db.prune_expired_share_links(now) {
            warn!("Failed to prune expired share links: {err:#}");
        }
        let exports_dir = self.media_dir.join("exports");
        let clips_dir = self.media_dir.join("clips");
        for target_id in expired_or_revoked_targets {
            let safe_id: String = target_id
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
                .collect();
            if safe_id.is_empty() || safe_id != target_id || target_id.contains("..") {
                continue;
            }
            if !self
                .db
                .has_active_share_links_for_target(&target_id, now)
                .unwrap_or(false)
                && !self
                    .db
                    .has_recently_accessed_share_links_for_target(&target_id, now, 900.0)
                    .unwrap_or(false)
            {
                for path in [
                    exports_dir.join(format!("share_redacted_{safe_id}.mp4")),
                    exports_dir.join(format!(".share_redacted_{safe_id}.mask_hash")),
                    exports_dir.join(format!("share_clip_{safe_id}.mp4")),
                    clips_dir.join(format!("share_redacted_{safe_id}.webp")),
                    clips_dir.join(format!("share_clip_{safe_id}.webp")),
                ] {
                    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file()) {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
        }
    }

    async fn run_preview_generator_loop(&self) {
        let ffmpeg_bin = self.ffmpeg_bin.clone();
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => {}
            _ = self.shutdown_notify.notified() => return,
        }

        loop {
            if self.shutdown_flag.load(Ordering::SeqCst) {
                break;
            }
            let cameras: Vec<(String, String)> = {
                let cfg = self.config.read().await;
                cfg.cameras
                    .iter()
                    .filter(|(_, c)| {
                        c.enabled
                            && c.record.enabled
                            && !ninjanvr_config::is_preview_disabled(&c.record.preview.quality)
                    })
                    .map(|(n, c)| (n.clone(), c.record.preview.quality.clone()))
                    .collect()
            };

            let now = unix_now_f64();
            let current_hour_start = ((now as i64) / 3600) * 3600;
            let prev_hour_start = current_hour_start - 3600;

            for (camera, preview_quality) in cameras {
                if !is_safe_identifier(&camera) {
                    continue;
                }
                let (vf_filter, crf_val) = match preview_quality.as_str() {
                    "very_high" => ("fps=2,scale=-2:480", "23"),
                    "high" => ("fps=1,scale=-2:360", "26"),
                    "medium" => ("fps=1,scale=-2:240", "28"),
                    _ => ("fps=1,scale=-2:180", "32"),
                };
                for hour_start_i64 in [prev_hour_start, current_hour_start] {
                    let hour_start = hour_start_i64 as f64;
                    let hour_end = (hour_start_i64 + 3600) as f64;
                    let file_name = format!("{hour_start_i64}-{}.mp4", hour_start_i64 + 3600);
                    let prev_nonce: u64 = rand::random();
                    let preview_dir = self.media_dir.join("clips").join("previews").join(&camera);
                    let list_file = preview_dir.join(format!(
                        ".prev_{camera}_{hour_start_i64}_{prev_nonce:016x}_concat.txt"
                    ));
                    let tmp_out_file =
                        preview_dir.join(format!(".{file_name}.{prev_nonce:016x}.tmp.mp4"));
                    let _list_guard = TempPathGuard(list_file.clone());
                    let _tmp_out_guard = TempPathGuard(tmp_out_file.clone());

                    let db_prep = self.db.clone();
                    let media_dir_prep = self.media_dir.clone();
                    let preview_dir_prep = preview_dir.clone();
                    let cam_prep = camera.clone();
                    let file_name_prep = file_name.clone();
                    let list_file_prep = list_file.clone();
                    let prep_res =
                        tokio::task::spawn_blocking(move || -> Option<(PathBuf, f64, f64, f64)> {
                            let segs = db_prep
                                .query_recordings(&cam_prep, hour_start, hour_end)
                                .ok()?;
                            if segs.len() < 2 {
                                return None;
                            }
                            let start_ts = segs
                                .first()
                                .map_or(hour_start, |s| s.start_time.max(hour_start));
                            let end_ts = segs
                                .last()
                                .map_or(hour_end.min(now), |s| s.end_time.min(hour_end));
                            let mut covered_dur = 0.0f64;
                            let mut cov_cursor = hour_start;
                            for seg in &segs {
                                let s0 = seg.start_time.max(hour_start).max(cov_cursor);
                                let s1 = seg.end_time.min(hour_end);
                                if s1 > s0 {
                                    covered_dur += s1 - s0;
                                    cov_cursor = s1;
                                }
                            }
                            let duration = covered_dur.max(1.0);

                            if std::fs::create_dir_all(&preview_dir_prep).is_err() {
                                return None;
                            }
                            let out_file = preview_dir_prep.join(&file_name_prep);
                            if let Ok(meta) = std::fs::symlink_metadata(&out_file)
                                && meta.file_type().is_file()
                                && meta.len() > 1024
                            {
                                if hour_start_i64 == prev_hour_start {
                                    return None;
                                }
                                if let Ok(mtime) = meta.modified()
                                    && mtime.elapsed().is_ok_and(|d| d.as_secs() < 900)
                                {
                                    return None;
                                }
                            }
                            let canon_media_dir = std::fs::canonicalize(&media_dir_prep).ok();
                            let mut list_content = String::new();
                            for seg in &segs {
                                let local_path = if let Some(rel) = strip_media_root(&seg.path) {
                                    media_dir_prep.join(rel)
                                } else {
                                    PathBuf::from(&seg.path)
                                };
                                if std::fs::symlink_metadata(&local_path)
                                    .is_ok_and(|m| m.file_type().is_file())
                                    && let (Some(root_canon), Ok(seg_canon)) =
                                        (&canon_media_dir, std::fs::canonicalize(&local_path))
                                    && seg_canon.starts_with(root_canon)
                                    && seg_canon.is_file()
                                    && let Some(line) = format_ffmpeg_concat_file_line(&seg_canon)
                                {
                                    list_content.push_str(&line);
                                }
                            }
                            if list_content.is_empty() {
                                return None;
                            }
                            let mut lf = create_new_file_nofollow(&list_file_prep).ok()?;
                            if std::io::Write::write_all(&mut lf, list_content.as_bytes()).is_err()
                            {
                                return None;
                            }
                            Some((out_file, start_ts, end_ts, duration))
                        })
                        .await
                        .ok()
                        .flatten();

                    let Some((out_file, start_ts, end_ts, duration)) = prep_res else {
                        continue;
                    };

                    let mut cmd = Command::new(&ffmpeg_bin);
                    cmd.args([
                        "-y",
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-nostdin",
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
                        "veryfast",
                        "-crf",
                        crf_val,
                        "-an",
                        "-f",
                        "mp4",
                        "-movflags",
                        "+faststart",
                    ])
                    .arg(sanitize_ffmpeg_path_arg(&tmp_out_file))
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                    let status = cmd.status().await;
                    let ff_ok = status.is_ok_and(|s| s.success());
                    let db_fin = self.db.clone();
                    let cam_fin = camera.clone();
                    let list_fin = list_file.clone();
                    let tmp_out_fin = tmp_out_file.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = std::fs::remove_file(&list_fin);
                        if ff_ok
                            && std::fs::symlink_metadata(&tmp_out_fin)
                                .is_ok_and(|m| m.file_type().is_file() && m.len() > 0)
                            && std::fs::rename(&tmp_out_fin, &out_file).is_ok()
                        {
                            let id = format!("{cam_fin}_{hour_start_i64}");
                            let db_path =
                                format!("/media/ninjanvr/clips/previews/{cam_fin}/{file_name}");
                            let _ = db_fin.insert_preview(
                                &id, &cam_fin, &db_path, start_ts, end_ts, duration,
                            );
                        } else {
                            let _ = std::fs::remove_file(&tmp_out_fin);
                        }
                    })
                    .await;
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(120)) => {}
                _ = self.shutdown_notify.notified() => break,
            }
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
    if cold_dir.to_str().is_none() {
        anyhow::bail!("cold tier directory path is not valid UTF-8");
    }
    if let Err(err) = std::fs::create_dir_all(cold_dir) {
        anyhow::bail!(
            "cold tier directory {} unreachable: {err}",
            cold_dir.display()
        );
    }
    let canon_cold_root = std::fs::canonicalize(cold_dir).with_context(|| {
        format!(
            "failed to canonicalize cold tier dir {}",
            cold_dir.display()
        )
    })?;
    let hot_rec_root = hot_media_dir.join("recordings");
    let canon_hot_root = std::fs::canonicalize(&hot_rec_root).ok();
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
            let rel_path = Path::new(rel_under_rec);
            if rel_under_rec.is_empty()
                || rel_path
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                warn!("Skipping tier migration for unsafe recording path: {db_path}");
                continue;
            }
            let src_path = hot_rec_root.join(rel_path);
            let Ok(src_meta) = std::fs::symlink_metadata(&src_path) else {
                let _ = db.delete_recording_by_id(&id);
                continue;
            };
            if !src_meta.file_type().is_file() {
                warn!(
                    "Skipping tier migration for non-regular file {}",
                    src_path.display()
                );
                continue;
            }
            if let (Some(root_c), Ok(src_c)) = (&canon_hot_root, std::fs::canonicalize(&src_path))
                && !src_c.starts_with(root_c)
            {
                warn!(
                    "Skipping tier migration for path outside hot root: {}",
                    src_path.display()
                );
                continue;
            }
            let mut src_opts = std::fs::OpenOptions::new();
            src_opts.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                src_opts.custom_flags(O_NOFOLLOW_FLAG);
            }
            let mut src_file = match src_opts.open(&src_path) {
                Ok(f) => f,
                Err(err) => {
                    warn!(
                        "Skipping tier migration for unreadable or symlink source {}: {err}",
                        src_path.display()
                    );
                    continue;
                }
            };
            let Ok(src_fd_meta) = src_file.metadata() else {
                continue;
            };
            if !src_fd_meta.file_type().is_file() {
                warn!(
                    "Skipping tier migration for non-regular source descriptor {}",
                    src_path.display()
                );
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if src_fd_meta.dev() != src_meta.dev() || src_fd_meta.ino() != src_meta.ino() {
                    warn!(
                        "Skipping tier migration due to source inode mismatch on {}",
                        src_path.display()
                    );
                    continue;
                }
            }
            let dst_path = cold_dir.join(rel_path);
            let Some(new_db_path) = dst_path.to_str().map(str::to_string) else {
                anyhow::bail!("cold tier destination path is not valid UTF-8");
            };
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
            let Ok(canon_dst_parent) = std::fs::canonicalize(dst_parent) else {
                continue;
            };
            if !canon_dst_parent.starts_with(&canon_cold_root) {
                warn!(
                    "Skipping tier migration for destination escaping cold root: {}",
                    dst_parent.display()
                );
                continue;
            }
            if std::fs::symlink_metadata(&dst_path).is_ok_and(|m| !m.file_type().is_file()) {
                warn!(
                    "Skipping tier migration for non-regular destination {}",
                    dst_path.display()
                );
                continue;
            }
            let nonce: u64 = rand::random();
            let file_name = dst_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("seg.mp4");
            let tmp_dst = dst_parent.join(format!(".{file_name}.{nonce:016x}.tmp"));
            let _tmp_guard = TempPathGuard(tmp_dst.clone());
            let copy_res = (|| -> std::io::Result<()> {
                let mut dst_file = create_new_file_nofollow(&tmp_dst)?;
                std::io::copy(&mut src_file, &mut dst_file)?;
                dst_file.sync_all()?;
                Ok(())
            })();
            if let Err(err) = copy_res {
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
            if db.update_recording_path(&id, &new_db_path)? {
                if std::fs::symlink_metadata(&src_path).is_ok_and(|m| {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        m.file_type().is_file()
                            && m.dev() == src_meta.dev()
                            && m.ino() == src_meta.ino()
                    }
                    #[cfg(not(unix))]
                    {
                        m.file_type().is_file()
                    }
                }) {
                    let _ = std::fs::remove_file(&src_path);
                    prune_empty_parent_dirs(&src_path, &hot_rec_root);
                }
                migrated += 1;
            }
        }
        if batch_len < 200 {
            break;
        }
    }
    Ok(migrated)
}

/// Builds the millisecond-precision stem and nonce-suffixed temporary paths for `backup_alert_clip`.
pub fn build_alert_backup_paths(
    day_dir: &Path,
    camera: &str,
    label: &str,
    start_time: f64,
    nonce: u64,
) -> (String, PathBuf, PathBuf, PathBuf) {
    let safe_camera: String = camera
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe_label: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let safe_start = if start_time.is_finite() {
        start_time.clamp(0.0, 4_102_444_800.0)
    } else {
        0.0
    };
    let start_ms = (safe_start * 1000.0).round() as u64;
    let stem = format!("{safe_camera}_{start_ms}_{safe_label}");
    let out_mp4 = day_dir.join(format!("{stem}.mp4"));
    let list_path = day_dir.join(format!(".{stem}_{nonce:016x}_concat.txt"));
    let tmp_mp4 = day_dir.join(format!(".{stem}_{nonce:016x}.mp4.tmp"));
    (stem, out_mp4, list_path, tmp_mp4)
}

/// Automatically exports/copies a high-severity Alert clip and its best snapshot
/// into a secondary/offsite directory `target_dir/<YYYY-MM-DD>/<camera>_<ts_ms>_<label>.mp4`.
// Internal media pipeline helper passing distinct FFmpeg/segment parameters directly
#[expect(clippy::too_many_arguments)]
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
    if !is_safe_identifier(camera) {
        anyhow::bail!("unsafe camera identifier for alert backup: {camera}");
    }
    let safe_start = if start_time.is_finite() {
        start_time.clamp(0.0, 4_102_444_800.0)
    } else {
        0.0
    };
    let dt = Utc
        .timestamp_opt(safe_start as i64, 0)
        .single()
        .unwrap_or_else(Utc::now);
    let date_dir = dt.format("%Y-%m-%d").to_string();
    let day_dir = target_dir.join(&date_dir);

    let nonce: u64 = rand::random();
    let (stem, out_mp4, list_path, tmp_mp4) =
        build_alert_backup_paths(&day_dir, camera, label, start_time, nonce);
    let _list_guard = TempPathGuard(list_path.clone());
    let _tmp_mp4_guard = TempPathGuard(tmp_mp4.clone());

    let db_prep = db.clone();
    let media_dir_prep = media_dir.to_path_buf();
    let day_dir_prep = day_dir.clone();
    let cam_prep = camera.to_string();
    let out_mp4_prep = out_mp4.clone();
    let tmp_mp4_prep = tmp_mp4.clone();
    let list_path_prep = list_path.clone();
    let existing_paths = tokio::task::spawn_blocking(move || -> Result<Vec<PathBuf>> {
        std::fs::create_dir_all(&day_dir_prep).with_context(|| {
            format!(
                "failed to create alert backup dir {}",
                day_dir_prep.display()
            )
        })?;
        let mut segments = db_prep.query_recordings(&cam_prep, start_time, end_time)?;
        if segments.is_empty() {
            segments = db_prep.query_recordings(&cam_prep, start_time - 15.0, end_time + 15.0)?;
        }
        let canon_media_dir = std::fs::canonicalize(&media_dir_prep).ok();
        let existing_paths: Vec<PathBuf> = segments
            .iter()
            .filter_map(|s| {
                let local = if let Some(rel) = strip_media_root(&s.path) {
                    media_dir_prep.join(rel)
                } else {
                    PathBuf::from(&s.path)
                };
                let lmeta = std::fs::symlink_metadata(&local).ok()?;
                if !lmeta.file_type().is_file() {
                    return None;
                }
                let canon = std::fs::canonicalize(&local).ok()?;
                if let Some(ref root_c) = canon_media_dir
                    && !canon.starts_with(root_c)
                {
                    return None;
                }
                Some(canon)
            })
            .collect();

        let Some(first_src) = existing_paths.first() else {
            anyhow::bail!("no recording segments found to back up for {cam_prep}");
        };

        if existing_paths.len() == 1 {
            copy_file_via_nofollow(first_src, &tmp_mp4_prep).with_context(|| {
                format!("failed to copy alert clip to {}", tmp_mp4_prep.display())
            })?;
            std::fs::rename(&tmp_mp4_prep, &out_mp4_prep).with_context(|| {
                format!("failed to copy alert clip to {}", out_mp4_prep.display())
            })?;
        } else {
            let mut list_content = String::new();
            for p in &existing_paths {
                if let Some(line) = format_ffmpeg_concat_file_line(p) {
                    list_content.push_str(&line);
                }
            }
            let mut lf = create_new_file_nofollow(&list_path_prep).with_context(|| {
                format!("failed to write concat list {}", list_path_prep.display())
            })?;
            std::io::Write::write_all(&mut lf, list_content.as_bytes()).with_context(|| {
                format!("failed to write concat list {}", list_path_prep.display())
            })?;
        }
        Ok(existing_paths)
    })
    .await
    .context("alert backup preparation task panicked")??;

    let remux_needed_fallback = if existing_paths.len() > 1 {
        let dur = if (end_time - start_time).is_finite() {
            (end_time - start_time).clamp(10.0, 3600.0)
        } else {
            10.0
        };
        let ff_args = build_mp4_export_ffmpeg_args(&list_path, 0.0, dur, true, &tmp_mp4);
        let ffmpeg_bin = resolve_ffmpeg_bin();
        let mut cmd = Command::new(&ffmpeg_bin);
        cmd.args(&ff_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let remux_ok = cmd.status().await.is_ok_and(|st| st.success());
        !remux_ok
    } else {
        false
    };

    let media_dir_fin = media_dir.to_path_buf();
    let day_dir_fin = day_dir.clone();
    let out_mp4_fin = out_mp4.clone();
    let tmp_mp4_fin = tmp_mp4.clone();
    let list_path_fin = list_path.clone();
    let thumb_rel_owned = thumb_rel.map(|s| s.to_string());
    tokio::task::spawn_blocking(move || -> Result<PathBuf> {
        let _ = std::fs::remove_file(&list_path_fin);
        if existing_paths.len() > 1 {
            let valid_tmp = !remux_needed_fallback
                && std::fs::symlink_metadata(&tmp_mp4_fin)
                    .is_ok_and(|m| m.file_type().is_file() && m.len() > 0);
            if valid_tmp {
                std::fs::rename(&tmp_mp4_fin, &out_mp4_fin).with_context(|| {
                    format!(
                        "failed to publish alert backup clip to {}",
                        out_mp4_fin.display()
                    )
                })?;
            } else {
                let _ = std::fs::remove_file(&tmp_mp4_fin);
                copy_file_via_nofollow(&existing_paths[0], &tmp_mp4_fin).with_context(|| {
                    format!("failed to copy alert clip to {}", tmp_mp4_fin.display())
                })?;
                std::fs::rename(&tmp_mp4_fin, &out_mp4_fin).with_context(|| {
                    format!("failed to copy alert clip to {}", out_mp4_fin.display())
                })?;
            }
        }

        if include_snapshot && let Some(ref t_rel) = thumb_rel_owned {
            let canon_media_dir = std::fs::canonicalize(&media_dir_fin).ok();
            let thumb_disk = if let Some(rel) = strip_media_root(t_rel) {
                media_dir_fin.join(rel)
            } else {
                PathBuf::from(t_rel)
            };
            if let Ok(lmeta) = std::fs::symlink_metadata(&thumb_disk)
                && lmeta.file_type().is_file()
                && let (Some(root_c), Ok(canon_thumb)) =
                    (&canon_media_dir, std::fs::canonicalize(&thumb_disk))
                && canon_thumb.starts_with(root_c)
            {
                let ext: String = canon_thumb
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("webp")
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .collect();
                let safe_ext = if ext.is_empty() { "webp" } else { &ext };
                let snap_dst = day_dir_fin.join(format!("{stem}.{safe_ext}"));
                let tmp_snap = day_dir_fin.join(format!(".{stem}_{nonce:016x}.{safe_ext}.tmp"));
                let _tmp_snap_guard = TempPathGuard(tmp_snap.clone());
                let snap_res = copy_file_via_nofollow(&canon_thumb, &tmp_snap)
                    .and_then(|_| std::fs::rename(&tmp_snap, &snap_dst));
                if let Err(err) = snap_res {
                    warn!(
                        "Failed to copy alert snapshot {} to {}: {err}",
                        canon_thumb.display(),
                        snap_dst.display()
                    );
                }
            }
        }
        Ok(out_mp4_fin)
    })
    .await
    .context("alert backup finalize task panicked")?
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
    let safe_trim = if trim_offset.is_finite() && trim_offset > 0.0 {
        trim_offset
    } else {
        0.0
    };
    let safe_dur = if target_dur.is_finite() && target_dur > 0.0 {
        target_dur.clamp(0.1, 86_400.0)
    } else {
        3.0
    };
    let mut args = vec![
        "-y".to_string(),
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
    ];
    if safe_trim > 0.05 {
        args.push("-ss".to_string());
        args.push(format!("{safe_trim:.2}"));
    }
    args.extend([
        "-f".to_string(),
        "concat".to_string(),
        "-safe".to_string(),
        "0".to_string(),
        "-i".to_string(),
        sanitize_ffmpeg_path_arg(list_file),
        "-t".to_string(),
        format!("{safe_dur:.2}"),
        "-fflags".to_string(),
        "+genpts+igndts".to_string(),
        "-avoid_negative_ts".to_string(),
        "make_zero".to_string(),
    ]);
    if let Some(vf) = build_privacy_drawbox_filter(privacy_mask) {
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
        "-f".to_string(),
        "mp4".to_string(),
        "-movflags".to_string(),
        "+faststart".to_string(),
        sanitize_ffmpeg_path_arg(out_file),
    ]);
    args
}

/// Returns `true` when `mask` contains a non-empty privacy mask configuration.
pub fn privacy_mask_is_configured(mask: Option<&serde_json::Value>) -> bool {
    ninjanvr_vision::PrivacyMaskPolicy::from_optional_value(mask, 0, 0).is_configured()
}

/// Converts a `privacy_mask` config value (normalized or pixel polygon strings, coordinate-pair
/// arrays, or objects) into an FFmpeg `drawbox` video filter expression
/// (`drawbox=x=iw*0.2:y=ih*0.2:w=iw*0.6:h=ih*0.6:color=#0c0e12@1.0:t=fill`)
/// for redacting privacy regions during MP4 export transcoding.
pub fn build_privacy_drawbox_filter(mask: Option<&serde_json::Value>) -> Option<String> {
    ninjanvr_vision::PrivacyMaskPolicy::from_optional_value(mask, 0, 0)
        .ffmpeg_drawbox_filter()
        .ok()
        .flatten()
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
// Internal media pipeline helper passing distinct FFmpeg/segment parameters directly
#[expect(clippy::too_many_arguments)]
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

/// Parameters for [`build_mp4_export_with_progress`], supporting cold-tier segment resolution,
/// external cancellation tokens, and real-time FFmpeg progress callbacks.
pub struct Mp4ExportRequest<'a> {
    pub db: &'a NinjaDatabase,
    pub media_dir: &'a Path,
    pub cold_dir: Option<&'a Path>,
    pub export_id: &'a str,
    pub camera: &'a str,
    pub name: &'a str,
    pub start_time: f64,
    pub end_time: f64,
    pub include_audio: bool,
    pub privacy_mask: Option<&'a serde_json::Value>,
    pub cancel_token: Option<Arc<AtomicBool>>,
    pub on_progress: Option<Arc<dyn Fn(f64) + Send + Sync>>,
}

/// Checks whether `path` is a regular ISO-BMFF MP4 file containing a top-level `moov` box
/// (distinguishing real recorded segments from synthetic unit-test `ftyp`-only stubs).
pub fn mp4_file_has_moov_box(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    let Ok((mut file, meta)) = open_regular_file_nofollow(path) else {
        return false;
    };
    let file_len = meta.len();
    if file_len < 16 {
        return false;
    }
    let mut pos = 0u64;
    let mut hdr = [0u8; 8];
    let mut boxes_seen = 0usize;
    while pos.checked_add(8).is_some_and(|end| end <= file_len) {
        if boxes_seen >= MAX_MP4_BOXES_PER_LEVEL {
            return false;
        }
        boxes_seen += 1;
        if file.seek(SeekFrom::Start(pos)).is_err() || file.read_exact(&mut hdr).is_err() {
            return false;
        }
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let btype = [hdr[4], hdr[5], hdr[6], hdr[7]];
        if boxes_seen == 1 && !is_known_iso_top_level_box(&btype) {
            return false;
        }
        let (box_size, hdr_len) = if size32 == 1 {
            if pos.checked_add(16).is_none_or(|end| end > file_len) {
                return false;
            }
            let mut ext = [0u8; 8];
            if file.read_exact(&mut ext).is_err() {
                return false;
            }
            (u64::from_be_bytes(ext), 16u64)
        } else if size32 == 0 {
            (file_len - pos, 8u64)
        } else {
            (size32, 8u64)
        };
        let Some(next_pos) = pos.checked_add(box_size) else {
            return false;
        };
        if box_size < hdr_len || next_pos > file_len {
            return false;
        }
        if &btype == b"moov" && box_size > hdr_len {
            return true;
        }
        pos = next_pos;
    }
    false
}

/// Resolves a database recording segment path across both `media_dir` (hot tier) and optional
/// `cold_dir` (cold archive tier), rejecting symlinks, path traversal, empty files, and cross-camera paths.
pub fn resolve_export_segment_path(
    media_dir: &Path,
    cold_dir: Option<&Path>,
    camera: &str,
    db_path: &str,
) -> Option<PathBuf> {
    if db_path.is_empty()
        || Path::new(db_path)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    let verify_contained =
        |cand: &Path, root_canon: &Path, check_cam_under_rec: Option<&Path>| -> Option<PathBuf> {
            if !cand
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
            {
                return None;
            }
            let lmeta = std::fs::symlink_metadata(cand).ok()?;
            if !lmeta.file_type().is_file() || lmeta.len() < 16 {
                return None;
            }
            let canon_parent = std::fs::canonicalize(cand.parent()?).ok()?;
            let canon_file = std::fs::canonicalize(cand).ok()?;
            if canon_file.parent() != Some(canon_parent.as_path())
                || !canon_file.starts_with(root_canon)
            {
                return None;
            }
            let cmeta = std::fs::symlink_metadata(&canon_file).ok()?;
            if !cmeta.file_type().is_file() {
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if cmeta.dev() != lmeta.dev() || cmeta.ino() != lmeta.ino() {
                    return None;
                }
            }
            if let Some(cam_scope_root) = check_cam_under_rec
                && let Ok(rel) = canon_file.strip_prefix(cam_scope_root)
                && let Some(parent) = rel.parent()
                && !parent.as_os_str().is_empty()
                && !camera.is_empty()
                && !parent.components().any(|c| c.as_os_str() == camera)
            {
                return None;
            }
            Some(canon_file)
        };

    let hot_disk_path = if let Some(rel) = strip_media_root(db_path) {
        media_dir.join(rel)
    } else {
        PathBuf::from(db_path)
    };
    if std::fs::symlink_metadata(media_dir).is_ok_and(|m| !m.file_type().is_symlink())
        && let Ok(canon_media) = std::fs::canonicalize(media_dir)
    {
        let canon_rec = std::fs::canonicalize(media_dir.join("recordings")).ok();
        if let Some(found) = verify_contained(&hot_disk_path, &canon_media, canon_rec.as_deref()) {
            return Some(found);
        }
    }

    let cold_root = cold_dir?;
    if std::fs::symlink_metadata(cold_root).is_ok_and(|m| m.file_type().is_symlink()) {
        return None;
    }
    let canon_cold = std::fs::canonicalize(cold_root).ok()?;
    let mut cold_candidates = vec![PathBuf::from(db_path), hot_disk_path];
    if let Some(rel) = strip_media_root(db_path) {
        let rel_stripped = rel.strip_prefix("recordings/").unwrap_or(rel);
        cold_candidates.push(cold_root.join(rel_stripped));
        cold_candidates.push(cold_root.join(rel));
    }
    for cand in cold_candidates {
        if let Some(found) = verify_contained(&cand, &canon_cold, Some(canon_cold.as_path())) {
            return Some(found);
        }
    }
    None
}

/// Checks whether `path` contains a real encoded video sample entry (`avc1`/`hvc1`/`hev1`/`mp4v`/`vp09`/`av01`)
/// inside its `moov` box (distinguishing real recorded camera segments from synthetic test stubs).
pub fn mp4_file_has_real_video_codec(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    let Ok((mut file, meta)) = open_regular_file_nofollow(path) else {
        return false;
    };
    let file_len = meta.len();
    if file_len < 32 {
        return false;
    }
    let mut pos = 0u64;
    let mut hdr = [0u8; 8];
    let mut boxes_seen = 0usize;
    while pos.checked_add(8).is_some_and(|end| end <= file_len) {
        if boxes_seen >= MAX_MP4_BOXES_PER_LEVEL {
            return false;
        }
        boxes_seen += 1;
        if file.seek(SeekFrom::Start(pos)).is_err() || file.read_exact(&mut hdr).is_err() {
            return false;
        }
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let btype = [hdr[4], hdr[5], hdr[6], hdr[7]];
        if boxes_seen == 1 && !is_known_iso_top_level_box(&btype) {
            return false;
        }
        let (box_size, hdr_len) = if size32 == 1 {
            if pos.checked_add(16).is_none_or(|end| end > file_len) {
                return false;
            }
            let mut ext = [0u8; 8];
            if file.read_exact(&mut ext).is_err() {
                return false;
            }
            (u64::from_be_bytes(ext), 16u64)
        } else if size32 == 0 {
            (file_len - pos, 8u64)
        } else {
            (size32, 8u64)
        };
        let Some(next_pos) = pos.checked_add(box_size) else {
            return false;
        };
        if box_size < hdr_len || next_pos > file_len {
            return false;
        }
        if &btype == b"moov" {
            let payload_len = box_size - hdr_len;
            if payload_len > 16 * 1024 * 1024 {
                return false;
            }
            let mut moov = vec![0u8; payload_len as usize];
            if file.read_exact(&mut moov).is_err() {
                return false;
            }
            return moov.windows(4).any(|w| {
                matches!(
                    w,
                    b"avc1" | b"avc3" | b"hvc1" | b"hev1" | b"mp4v" | b"vp09" | b"av01"
                )
            });
        }
        pos = next_pos;
    }
    false
}

/// Extracts a single JPEG frame at `offset_sec` from a validated MP4 file (`segment_path`),
/// scaling to `max_width` (`160..=1280`) while preserving aspect ratio.
pub async fn extract_historical_frame_jpeg(
    segment_path: &Path,
    offset_sec: f64,
    duration_hint_sec: Option<f64>,
    max_width: u32,
) -> Result<Vec<u8>> {
    const MAX_FRAME_STDOUT_BYTES: usize = 2 * 1024 * 1024;
    let lmeta = std::fs::symlink_metadata(segment_path)
        .with_context(|| format!("segment not found: {}", segment_path.display()))?;
    if !lmeta.file_type().is_file() || lmeta.len() < 64 {
        anyhow::bail!("segment is not a valid regular file");
    }
    let safe_w = max_width.clamp(160, 1280);
    let safe_offset = if offset_sec.is_finite() && offset_sec > 0.0 {
        offset_sec.min(86_400.0)
    } else {
        0.0
    };
    if let Some(dur) = duration_hint_sec.filter(|d| d.is_finite() && *d > 0.0)
        && safe_offset > dur + 1.0
    {
        anyhow::bail!(
            "requested frame offset +{safe_offset:.2}s exceeds segment duration {dur:.2}s"
        );
    }
    let vf = format!("scale={safe_w}:-2");
    let ffmpeg_bin = resolve_ffmpeg_bin();
    let seg_arg = sanitize_ffmpeg_path_arg(segment_path);

    let run_once = |seek_args: Vec<String>| {
        let ffmpeg_bin = ffmpeg_bin.clone();
        let seg_arg = seg_arg.clone();
        let vf = vf.clone();
        async move {
            let mut cmd = Command::new(&ffmpeg_bin);
            cmd.args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-threads",
                "2",
            ]);
            cmd.args(&seek_args);
            cmd.arg("-i")
                .arg(&seg_arg)
                .args([
                    "-an",
                    "-sn",
                    "-dn",
                    "-frames:v",
                    "1",
                    "-vf",
                    &vf,
                    "-q:v",
                    "5",
                    "-f",
                    "image2pipe",
                    "-vcodec",
                    "mjpeg",
                    "pipe:1",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let out = tokio::time::timeout(std::time::Duration::from_secs(8), cmd.output()).await;
            match out {
                Ok(Ok(o))
                    if o.status.success()
                        && o.stdout.len() >= 4
                        && o.stdout.len() <= MAX_FRAME_STDOUT_BYTES
                        && o.stdout[0] == 0xFF
                        && o.stdout[1] == 0xD8 =>
                {
                    Some(o.stdout)
                }
                _ => None,
            }
        }
    };

    let primary_seek = if safe_offset > 0.02 {
        vec!["-ss".to_string(), format!("{safe_offset:.3}")]
    } else {
        Vec::new()
    };
    if let Some(bytes) = run_once(primary_seek).await {
        return Ok(bytes);
    }
    // Only fall back to `-sseof -0.5` when `safe_offset` landed right on the tail boundary of a known segment.
    if let Some(dur) = duration_hint_sec.filter(|d| d.is_finite() && *d > 0.0)
        && safe_offset >= (dur - 1.5).max(0.0)
        && safe_offset <= dur + 1.0
        && let Some(bytes) = run_once(vec!["-sseof".to_string(), "-0.5".to_string()]).await
    {
        return Ok(bytes);
    }
    anyhow::bail!(
        "failed to decode frame at +{safe_offset:.2}s from {}",
        segment_path.display()
    )
}

/// Concatenates recording segments in `[start, end]` into a single MP4 export file,
/// optionally stripping audio and redacting `privacy_mask` regions via FFmpeg `drawbox`.
// Internal media pipeline helper passing distinct FFmpeg/segment parameters directly
#[expect(clippy::too_many_arguments)]
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
    build_mp4_export_with_progress(Mp4ExportRequest {
        db,
        media_dir,
        cold_dir: None,
        export_id,
        camera,
        name,
        start_time,
        end_time,
        include_audio,
        privacy_mask,
        cancel_token: None,
        on_progress: None,
    })
    .await
}

/// Concatenates recording segments into a single MP4 export file with cold-tier support,
/// external cancellation, and real-time FFmpeg progress callbacks.
pub async fn build_mp4_export_with_progress(req: Mp4ExportRequest<'_>) -> Result<()> {
    struct ExportCancelGuard {
        db: NinjaDatabase,
        export_id: String,
        cancelled: Arc<AtomicBool>,
        disarmed: bool,
    }
    impl Drop for ExportCancelGuard {
        fn drop(&mut self) {
            if !self.disarmed {
                self.cancelled.store(true, Ordering::SeqCst);
                if is_safe_identifier(&self.export_id) {
                    let _ = self.db.delete_export(&self.export_id);
                }
            }
        }
    }

    let cancelled = req
        .cancel_token
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let mut cancel_guard = ExportCancelGuard {
        db: req.db.clone(),
        export_id: req.export_id.to_string(),
        cancelled: cancelled.clone(),
        disarmed: false,
    };

    let res = build_mp4_export_inner(
        req.db,
        req.media_dir,
        req.cold_dir,
        req.export_id,
        req.camera,
        req.name,
        req.start_time,
        req.end_time,
        req.include_audio,
        req.privacy_mask,
        cancelled,
        req.on_progress,
    )
    .await;
    if res.is_ok() {
        cancel_guard.disarmed = true;
    }
    res
}

pub fn build_export_temp_paths(
    exports_dir: &Path,
    export_id: &str,
    nonce: u64,
) -> (PathBuf, PathBuf) {
    let safe_id: String = export_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe_id = safe_id.replace("..", "_");
    let tmp_out_file = exports_dir.join(format!(".{safe_id}.{nonce:016x}.tmp.mp4"));
    let list_file =
        std::env::temp_dir().join(format!("ninjanvr-export-{safe_id}-{nonce:016x}.txt"));
    (tmp_out_file, list_file)
}

async fn run_ffmpeg_export_with_progress(
    ffmpeg_bin: &Path,
    ffmpeg_args: &[String],
    target_dur: f64,
    cancelled: &Arc<AtomicBool>,
    on_progress: Option<&Arc<dyn Fn(f64) + Send + Sync>>,
) -> bool {
    use tokio::io::{AsyncBufReadExt, BufReader};

    if cancelled.load(Ordering::SeqCst) {
        return false;
    }
    let mut cmd = Command::new(ffmpeg_bin);
    cmd.args(["-progress", "pipe:1", "-nostats"])
        .args(ffmpeg_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    let stdout_opt = child.stdout.take();
    let mut lines_opt = stdout_opt.map(|out| BufReader::new(out).lines());
    let mut poll_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    poll_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = poll_tick.tick() => {
                if cancelled.load(Ordering::SeqCst) {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return false;
                }
            }
            line_res = async {
                match lines_opt.as_mut() {
                    Some(lines) => lines.next_line().await,
                    None => std::future::pending().await,
                }
            } => {
                match line_res {
                    Ok(Some(line)) => {
                        if let Some(cb) = on_progress
                            && let Some(raw_us) = line
                                .strip_prefix("out_time_us=")
                                .or_else(|| line.strip_prefix("out_time_ms="))
                            && let Ok(us) = raw_us.trim().parse::<i64>()
                            && us >= 0
                        {
                            let sec = us as f64 / 1_000_000.0;
                            let frac = (sec / target_dur.max(1.0)).clamp(0.0, 0.94);
                            cb(frac);
                        }
                    }
                    _ => {
                        lines_opt = None;
                    }
                }
            }
            status_res = child.wait() => {
                if cancelled.load(Ordering::SeqCst) {
                    return false;
                }
                return status_res.is_ok_and(|s| s.success());
            }
        }
    }
}

fn build_mp4_export_transcode_fallback_args(
    list_file: &Path,
    trim_offset: f64,
    target_dur: f64,
    include_audio: bool,
    privacy_mask: Option<&serde_json::Value>,
    out_file: &Path,
) -> Vec<String> {
    let safe_trim = if trim_offset.is_finite() && trim_offset > 0.0 {
        trim_offset
    } else {
        0.0
    };
    let safe_dur = if target_dur.is_finite() && target_dur > 0.0 {
        target_dur.clamp(0.1, 86_400.0)
    } else {
        3.0
    };
    let mut args = vec![
        "-y".to_string(),
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
    ];
    if safe_trim > 0.05 {
        args.push("-ss".to_string());
        args.push(format!("{safe_trim:.2}"));
    }
    args.extend([
        "-f".to_string(),
        "concat".to_string(),
        "-safe".to_string(),
        "0".to_string(),
        "-i".to_string(),
        sanitize_ffmpeg_path_arg(list_file),
        "-t".to_string(),
        format!("{safe_dur:.2}"),
        "-fflags".to_string(),
        "+genpts+igndts".to_string(),
        "-avoid_negative_ts".to_string(),
        "make_zero".to_string(),
    ]);
    if let Some(vf) = build_privacy_drawbox_filter(privacy_mask) {
        args.extend(["-vf".to_string(), vf]);
    }
    args.extend([
        "-c:v".to_string(),
        "libx264".to_string(),
        "-preset".to_string(),
        "veryfast".to_string(),
        "-crf".to_string(),
        "23".to_string(),
        "-pix_fmt".to_string(),
        "yuv420p".to_string(),
    ]);
    if include_audio {
        args.extend([
            "-c:a".to_string(),
            "aac".to_string(),
            "-b:a".to_string(),
            "128k".to_string(),
        ]);
    } else {
        args.push("-an".to_string());
    }
    args.extend([
        "-f".to_string(),
        "mp4".to_string(),
        "-movflags".to_string(),
        "+faststart".to_string(),
        sanitize_ffmpeg_path_arg(out_file),
    ]);
    args
}

// Internal media pipeline helper passing distinct FFmpeg/segment parameters directly
#[expect(clippy::too_many_arguments)]
async fn build_mp4_export_inner(
    db: &NinjaDatabase,
    media_dir: &Path,
    cold_dir: Option<&Path>,
    export_id: &str,
    camera: &str,
    name: &str,
    start_time: f64,
    end_time: f64,
    include_audio: bool,
    privacy_mask: Option<&serde_json::Value>,
    cancelled: Arc<AtomicBool>,
    on_progress: Option<Arc<dyn Fn(f64) + Send + Sync>>,
) -> Result<()> {
    struct ExportTempGuard {
        tmp_out_file: PathBuf,
        list_file: PathBuf,
        tmp_thumb_file: PathBuf,
        thumb_file: PathBuf,
        out_file: PathBuf,
        cancelled: Arc<AtomicBool>,
        disarmed: bool,
    }
    impl Drop for ExportTempGuard {
        fn drop(&mut self) {
            if !self.disarmed {
                self.cancelled.store(true, Ordering::SeqCst);
            }
            let _ = std::fs::remove_file(&self.list_file);
            let _ = std::fs::remove_file(&self.tmp_out_file);
            let _ = std::fs::remove_file(&self.tmp_thumb_file);
            if !self.disarmed {
                let _ = std::fs::remove_file(&self.thumb_file);
                let _ = std::fs::remove_file(&self.out_file);
            }
        }
    }

    if !is_safe_identifier(export_id) {
        anyhow::bail!("unsafe export_id: {export_id}");
    }
    if !is_safe_identifier(camera) {
        anyhow::bail!("unsafe camera identifier: {camera}");
    }
    if !start_time.is_finite() || !end_time.is_finite() || end_time < start_time {
        anyhow::bail!("invalid export time range [{start_time}, {end_time}]");
    }

    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    let nonce: u64 = rand::random();
    let out_file = exports_dir.join(format!("{export_id}.mp4"));
    let (tmp_out_file, list_file) = build_export_temp_paths(&exports_dir, export_id, nonce);
    let tmp_thumb_file = clips_dir.join(format!(".{export_id}.{nonce:016x}.tmp.webp"));
    let thumb_file = clips_dir.join(format!("{export_id}.webp"));
    let mut temp_guard = ExportTempGuard {
        tmp_out_file: tmp_out_file.clone(),
        list_file: list_file.clone(),
        tmp_thumb_file: tmp_thumb_file.clone(),
        thumb_file: thumb_file.clone(),
        out_file: out_file.clone(),
        cancelled: cancelled.clone(),
        disarmed: false,
    };

    if privacy_mask_is_configured(privacy_mask)
        && build_privacy_drawbox_filter(privacy_mask).is_none()
    {
        anyhow::bail!(
            "privacy mask is configured for {camera} but could not be compiled into a redaction filter"
        );
    }

    if let Some(ref cb) = on_progress {
        cb(0.02);
    }

    let db_prep = db.clone();
    let media_dir_prep = media_dir.to_path_buf();
    let cold_dir_prep = cold_dir.map(Path::to_path_buf);
    let exports_dir_prep = exports_dir.clone();
    let clips_dir_prep = clips_dir.clone();
    let cam_prep = camera.to_string();
    let exp_id_prep = export_id.to_string();
    let list_file_prep = list_file.clone();
    let tmp_out_prep = tmp_out_file.clone();
    let tmp_thumb_prep = tmp_thumb_file.clone();
    let cancel_prep = cancelled.clone();
    let (existing_paths, first_seg_start, any_has_moov, any_has_real_codec) =
        tokio::task::spawn_blocking(move || -> Result<(Vec<PathBuf>, Option<f64>, bool, bool)> {
            if cancel_prep.load(Ordering::SeqCst) {
                anyhow::bail!("export {exp_id_prep} cancelled");
            }
            let mut segments = db_prep.query_recordings(&cam_prep, start_time, end_time)?;
            if segments.is_empty() {
                segments =
                    db_prep.query_recordings(&cam_prep, start_time - 15.0, end_time + 15.0)?;
            }
            if segments.is_empty() {
                anyhow::bail!(
                    "no recording segments found for {cam_prep} in [{start_time}, {end_time}]"
                );
            }

            std::fs::create_dir_all(&exports_dir_prep)?;
            std::fs::create_dir_all(&clips_dir_prep)?;

            let mut list_content = String::new();
            let mut existing_paths = Vec::new();
            let mut first_seg_start = None;
            for seg in &segments {
                if let Some(seg_canon) = resolve_export_segment_path(
                    &media_dir_prep,
                    cold_dir_prep.as_deref(),
                    &cam_prep,
                    &seg.path,
                ) && let Some(line) = format_ffmpeg_concat_file_line(&seg_canon)
                {
                    if first_seg_start.is_none() {
                        first_seg_start = Some(seg.start_time);
                    }
                    list_content.push_str(&line);
                    existing_paths.push(seg_canon);
                }
            }

            if existing_paths.is_empty() {
                anyhow::bail!("no segment files exist on disk for export {exp_id_prep}");
            }
            if cancel_prep.load(Ordering::SeqCst) {
                anyhow::bail!("export {exp_id_prep} cancelled");
            }
            let any_has_moov = existing_paths
                .iter()
                .take(8)
                .any(|p| mp4_file_has_moov_box(p));
            let any_has_real_codec = existing_paths
                .iter()
                .take(8)
                .any(|p| mp4_file_has_real_video_codec(p));
            let mut lf = create_new_file_nofollow(&list_file_prep)?;
            std::io::Write::write_all(&mut lf, list_content.as_bytes())?;
            if cancel_prep.load(Ordering::SeqCst) {
                let _ = std::fs::remove_file(&list_file_prep);
                anyhow::bail!("export {exp_id_prep} cancelled");
            }
            let _ = std::fs::remove_file(&tmp_out_prep);
            let _ = std::fs::remove_file(&tmp_thumb_prep);
            Ok((
                existing_paths,
                first_seg_start,
                any_has_moov,
                any_has_real_codec,
            ))
        })
        .await
        .context("export preparation task panicked")??;

    let trim_offset = first_seg_start.map_or(0.0, |s0| (start_time - s0).max(0.0));
    let target_dur = (end_time - start_time).clamp(0.5, 86_400.0);

    let ffmpeg_bin = resolve_ffmpeg_bin();
    let ffmpeg_args = build_mp4_export_ffmpeg_args_with_privacy(
        &list_file,
        trim_offset,
        target_dur,
        include_audio,
        privacy_mask,
        &tmp_out_file,
    );
    let mut ff_ok = run_ffmpeg_export_with_progress(
        &ffmpeg_bin,
        &ffmpeg_args,
        target_dur,
        &cancelled,
        on_progress.as_ref(),
    )
    .await;

    let mask_configured = privacy_mask_is_configured(privacy_mask);
    let is_real_encoded_export = any_has_real_codec || (any_has_moov && existing_paths.len() > 1);
    // If lossless stream-copy failed on real recorded MP4s (e.g. G.711 audio in MP4 or
    // resolution/SPS/PPS switch across segments in a long export), retry once with libx264+aac.
    if !ff_ok && !cancelled.load(Ordering::SeqCst) && is_real_encoded_export && !mask_configured {
        let _ = std::fs::remove_file(&tmp_out_file);
        let fallback_args = build_mp4_export_transcode_fallback_args(
            &list_file,
            trim_offset,
            target_dur,
            include_audio,
            privacy_mask,
            &tmp_out_file,
        );
        ff_ok = run_ffmpeg_export_with_progress(
            &ffmpeg_bin,
            &fallback_args,
            target_dur,
            &cancelled,
            on_progress.as_ref(),
        )
        .await;
    }

    let list_file_v = list_file.clone();
    let tmp_out_v = tmp_out_file.clone();
    let exp_id_v = export_id.to_string();
    let cam_v = camera.to_string();
    let cancel_v = cancelled.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let _ = std::fs::remove_file(&list_file_v);
        if cancel_v.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(&tmp_out_v);
            anyhow::bail!("export {exp_id_v} cancelled");
        }
        let valid_tmp = ff_ok
            && std::fs::symlink_metadata(&tmp_out_v)
                .is_ok_and(|m| m.file_type().is_file() && m.len() >= 1024);
        if !valid_tmp {
            let _ = std::fs::remove_file(&tmp_out_v);
            if mask_configured {
                anyhow::bail!(
                    "failed to redact export {exp_id_v} with privacy mask for camera {cam_v}"
                );
            }
            if !include_audio && existing_paths.iter().any(|p| mp4_file_has_audio_track(p)) {
                anyhow::bail!(
                    "failed to strip audio track for export {exp_id_v} on camera {cam_v}"
                );
            }
            if is_real_encoded_export {
                anyhow::bail!(
                    "failed to remux {} recording segment(s) for export {exp_id_v} on camera {cam_v}",
                    existing_paths.len()
                );
            }
            copy_file_via_nofollow(&existing_paths[0], &tmp_out_v)?;
            if cancel_v.load(Ordering::SeqCst) {
                let _ = std::fs::remove_file(&tmp_out_v);
                anyhow::bail!("export {exp_id_v} cancelled");
            }
        }
        Ok(())
    })
    .await
    .context("export segment verification task panicked")??;

    if let Some(ref cb) = on_progress {
        cb(0.96);
    }

    if export_id.starts_with("share_redacted_") || export_id.starts_with("share_clip_") {
        let tmp_out_share = tmp_out_file.clone();
        let out_share = out_file.clone();
        let cancel_share = cancelled.clone();
        let exp_id_share = export_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<()> {
            if cancel_share.load(Ordering::SeqCst) {
                let _ = std::fs::remove_file(&tmp_out_share);
                anyhow::bail!("export {exp_id_share} cancelled");
            }
            std::fs::rename(&tmp_out_share, &out_share)?;
            if cancel_share.load(Ordering::SeqCst) {
                let _ = std::fs::remove_file(&out_share);
                anyhow::bail!("export {exp_id_share} cancelled");
            }
            Ok(())
        })
        .await
        .context("share export rename task panicked")??;
        temp_guard.disarmed = true;
        return Ok(());
    }

    // Extract a real video frame thumbnail from `tmp_out_file` into `tmp_thumb_file` BEFORE
    // publishing `out_file`, so cancellation/timeout during thumbnail extraction never orphans `out_file`.
    let thumb_vf = if let Some(db_f) = build_privacy_drawbox_filter(privacy_mask) {
        format!("{db_f},scale=320:-1")
    } else {
        "scale=320:-1".to_string()
    };
    struct ThumbChildGuard {
        child: Option<tokio::process::Child>,
        tmp_thumb_path: PathBuf,
    }
    impl Drop for ThumbChildGuard {
        fn drop(&mut self) {
            if let Some(mut c) = self.child.take() {
                let _ = c.start_kill();
                for _ in 0..40 {
                    if matches!(c.try_wait(), Ok(Some(_)) | Err(_)) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                let _ = std::fs::remove_file(&self.tmp_thumb_path);
            }
        }
    }

    let mut thumb_cmd = Command::new(&ffmpeg_bin);
    thumb_cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .args(["-y", "-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(sanitize_ffmpeg_path_arg(&tmp_out_file))
        .args(["-frames:v", "1", "-vf", &thumb_vf])
        .arg(sanitize_ffmpeg_path_arg(&tmp_thumb_file));
    let thumb_ok = match thumb_cmd.spawn() {
        Ok(child) => {
            let mut guard = ThumbChildGuard {
                child: Some(child),
                tmp_thumb_path: tmp_thumb_file.clone(),
            };
            let res = guard.child.as_mut().unwrap().wait().await;
            guard.child = None;
            res.is_ok_and(|s| s.success())
        }
        Err(_) => false,
    };
    let db_fin = db.clone();
    let exp_id_fin = export_id.to_string();
    let cam_fin = camera.to_string();
    let name_fin = name.to_string();
    let tmp_out_fin = tmp_out_file.clone();
    let out_fin = out_file.clone();
    let tmp_thumb_fin = tmp_thumb_file.clone();
    let thumb_fin = thumb_file.clone();
    let cancel_fin = cancelled.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        if cancel_fin.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(&tmp_out_fin);
            let _ = std::fs::remove_file(&tmp_thumb_fin);
            anyhow::bail!("export {exp_id_fin} cancelled");
        }
        if !thumb_ok || !tmp_thumb_fin.exists() {
            let mut img = image::RgbImage::new(320, 180);
            for p in img.pixels_mut() {
                *p = image::Rgb([24, 30, 38]);
            }
            if let Err(err) = img.save_with_format(&tmp_thumb_fin, image::ImageFormat::WebP) {
                warn!(
                    "Failed to save fallback export thumbnail {}: {err}",
                    tmp_thumb_fin.display()
                );
                let _ = std::fs::remove_file(&tmp_out_fin);
                let _ = std::fs::remove_file(&tmp_thumb_fin);
                return Err(err.into());
            }
        }
        if cancel_fin.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(&tmp_out_fin);
            let _ = std::fs::remove_file(&tmp_thumb_fin);
            anyhow::bail!("export {exp_id_fin} cancelled");
        }

        if let Err(err) = std::fs::rename(&tmp_thumb_fin, &thumb_fin) {
            warn!(
                "Failed to publish export thumbnail {}: {err}",
                thumb_fin.display()
            );
            let _ = std::fs::remove_file(&tmp_out_fin);
            let _ = std::fs::remove_file(&tmp_thumb_fin);
            return Err(err.into());
        }
        if let Err(err) = std::fs::rename(&tmp_out_fin, &out_fin) {
            let _ = std::fs::remove_file(&thumb_fin);
            return Err(err.into());
        }

        let video_rel = format!("/media/ninjanvr/exports/{exp_id_fin}.mp4");
        let thumb_rel = format!("/media/ninjanvr/clips/{exp_id_fin}.webp");
        if let Err(err) = db_fin.insert_export(&ninjanvr_db::ExportRow {
            id: exp_id_fin.clone(),
            camera: cam_fin,
            name: name_fin,
            date: unix_now_f64(),
            video_path: video_rel,
            thumb_path: thumb_rel,
            in_progress: false,
        }) {
            let _ = std::fs::remove_file(&out_fin);
            let _ = std::fs::remove_file(&thumb_fin);
            return Err(err);
        }

        if cancel_fin.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(&out_fin);
            let _ = std::fs::remove_file(&thumb_fin);
            let _ = db_fin.delete_export(&exp_id_fin);
            anyhow::bail!("export {exp_id_fin} cancelled");
        }
        Ok(())
    })
    .await
    .context("export publish task panicked")??;

    temp_guard.disarmed = true;
    if let Some(ref cb) = on_progress {
        cb(1.0);
    }
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
        assert!(
            stats.is_compressed,
            "expected mp4a stsd to be marked as compressed"
        );

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
            build_recorder_ffmpeg_args("rtsp://127.0.0.1:8554/cam", "/tmp/cam_seg_%09d.mp4", 0)
                .unwrap();
        assert!(rec_args_zero.iter().any(|a| a == "-nostdin"));
        assert!(
            rec_args_zero
                .windows(2)
                .any(|w| w == ["-protocol_whitelist", "rtsp,rtsps,rtp,tcp,udp,tls,crypto"])
        );
        assert!(!rec_args_zero.iter().any(|a| a == "-itsoffset"));
        assert!(!rec_args_zero.iter().any(|a| a == "-strftime"));

        let rec_args_offset =
            build_recorder_ffmpeg_args("rtsp://127.0.0.1:8554/cam", "/tmp/cam_seg_%09d.mp4", -450)
                .unwrap();
        assert!(rec_args_offset.iter().any(|a| a == "-nostdin"));
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
        let vf = build_privacy_drawbox_filter(Some(&mask_norm)).expect("expected drawbox filter");
        assert_eq!(
            vf,
            "drawbox=x=iw*0.100:y=ih*0.200:w=iw*0.400:h=ih*0.600:color=#0c0e12@1.0:t=fill"
        );

        let mask_px = json!("100,200,500,200,500,800,100,800");
        let vf_px =
            build_privacy_drawbox_filter(Some(&mask_px)).expect("expected pixel drawbox filter");
        assert_eq!(
            vf_px,
            "drawbox=x=100:y=200:w=400:h=600:color=#0c0e12@1.0:t=fill"
        );

        let mask_coord_pairs = json!([[[0.1, 0.1], [0.5, 0.5]]]);
        let vf_pairs =
            build_privacy_drawbox_filter(Some(&mask_coord_pairs)).expect("coordinate-pair filter");
        assert_eq!(
            vf_pairs,
            "drawbox=x=iw*0.100:y=ih*0.100:w=iw*0.400:h=ih*0.400:color=#0c0e12@1.0:t=fill"
        );

        let mask_single_poly_pairs = json!([[0.2, 0.2], [0.6, 0.2], [0.6, 0.7], [0.2, 0.7]]);
        let vf_single_pairs = build_privacy_drawbox_filter(Some(&mask_single_poly_pairs))
            .expect("single polygon coordinate-pair filter");
        assert_eq!(
            vf_single_pairs,
            "drawbox=x=iw*0.200:y=ih*0.200:w=iw*0.400:h=ih*0.500:color=#0c0e12@1.0:t=fill"
        );

        let mask_flat_nums = json!([0.1, 0.1, 0.4, 0.4]);
        let vf_flat =
            build_privacy_drawbox_filter(Some(&mask_flat_nums)).expect("flat number array filter");
        assert!(vf_flat.contains("drawbox=x=iw*0.100:y=ih*0.100:w=iw*0.300:h=ih*0.300"));

        let mask_zones_obj = json!({
            "zones": [
                { "points": [[0.1, 0.2], [0.5, 0.6]] }
            ]
        });
        let vf_zones =
            build_privacy_drawbox_filter(Some(&mask_zones_obj)).expect("zones/points filter");
        assert!(vf_zones.contains("drawbox=x=iw*0.100:y=ih*0.200:w=iw*0.400:h=ih*0.400"));

        assert!(!privacy_mask_is_configured(None));
        assert!(!privacy_mask_is_configured(Some(&json!(null))));
        assert!(!privacy_mask_is_configured(Some(&json!([]))));
        assert!(!privacy_mask_is_configured(Some(&json!("  "))));
        assert!(!privacy_mask_is_configured(Some(&json!({}))));
        assert!(privacy_mask_is_configured(Some(&mask_norm)));
        assert!(privacy_mask_is_configured(Some(&mask_px)));
        assert!(privacy_mask_is_configured(Some(&mask_zones_obj)));

        assert_eq!(build_privacy_drawbox_filter(None), None);
        assert_eq!(build_privacy_drawbox_filter(Some(&json!(null))), None);
        assert_eq!(build_privacy_drawbox_filter(Some(&json!([]))), None);
        assert_eq!(build_privacy_drawbox_filter(Some(&json!(""))), None);
        assert_eq!(
            build_privacy_drawbox_filter(Some(&json!(["0.1,0.2"]))),
            None
        );

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

    #[tokio::test]
    async fn build_mp4_export_with_privacy_fails_closed_when_mask_active_and_redaction_fails() {
        let root = std::env::temp_dir().join(format!(
            "ninjanvr-privacy-export-fail-closed-{}",
            random_suffix()
        ));
        let rec_dir = root.join("recordings/2026-11-03/12/front_door");
        std::fs::create_dir_all(&rec_dir).unwrap();
        let seg_path = rec_dir.join("00.00.mp4");
        std::fs::write(&seg_path, b"UNREDACTED_RAW_SEGMENT_BYTES").unwrap();

        let db = NinjaDatabase::open(root.join("ninjanvr.db")).unwrap();
        db.insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_fd_priv_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/00.00.mp4".to_string(),
            start_time: 1000.0,
            end_time: 1010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

        // 1. Invalid non-empty privacy mask -> fails closed with compilation error
        let invalid_mask = json!(["0.1,0.2"]);
        let err_invalid = build_mp4_export_with_privacy(
            &db,
            &root,
            "exp_invalid_mask",
            "front_door",
            "Invalid Mask Export",
            1000.0,
            1010.0,
            false,
            Some(&invalid_mask),
        )
        .await
        .expect_err("invalid non-empty privacy mask must fail closed");
        assert!(
            err_invalid
                .to_string()
                .contains("could not be compiled into a redaction filter"),
            "unexpected error: {err_invalid}"
        );
        assert!(!root.join("exports/exp_invalid_mask.mp4").exists());

        // 2. Valid privacy mask where ffmpeg cannot transcode raw segment -> fails closed without copying raw segment
        let valid_mask = json!([[[0.1, 0.1], [0.5, 0.5]]]);
        let err_redact = build_mp4_export_with_privacy(
            &db,
            &root,
            "exp_valid_mask",
            "front_door",
            "Redacted Export",
            1000.0,
            1010.0,
            false,
            Some(&valid_mask),
        )
        .await
        .expect_err("failed ffmpeg redaction must fail closed");
        assert!(
            err_redact
                .to_string()
                .contains("failed to redact export exp_valid_mask with privacy mask"),
            "unexpected error: {err_redact}"
        );
        assert!(!root.join("exports/exp_valid_mask.mp4").exists());
        assert!(db.get_export_by_id("exp_valid_mask").unwrap().is_none());

        // 3. Unique per-invocation temp paths & cleanup of .tmp.mp4 and ninjanvr-export-*.txt on error/completion
        let exports_dir = root.join("exports");
        let (tmp1, list1) =
            build_export_temp_paths(&exports_dir, "exp_concurrent", 0x1122334455667788);
        let (tmp2, list2) =
            build_export_temp_paths(&exports_dir, "exp_concurrent", 0x99aabbccddeeff00);
        assert_ne!(tmp1, tmp2);
        assert_ne!(list1, list2);
        assert_eq!(
            tmp1.file_name().unwrap().to_str().unwrap(),
            ".exp_concurrent.1122334455667788.tmp.mp4"
        );
        assert_eq!(
            list1.file_name().unwrap().to_str().unwrap(),
            "ninjanvr-export-exp_concurrent-1122334455667788.txt"
        );

        let leftover_tmps: Vec<_> = std::fs::read_dir(&exports_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp.mp4"))
            .collect();
        assert!(
            leftover_tmps.is_empty(),
            "all .tmp.mp4 files must be cleaned up on error"
        );

        let _ = std::fs::remove_dir_all(&root);
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
            audio_probe_enabled: false,
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

    #[test]
    fn build_cold_compaction_ffmpeg_args_supports_software_vaapi_qsv_nvenc_and_clamps_crf() {
        let src = Path::new("/media/hot/seg.mp4");
        let dst = Path::new("/media/cold/seg.mp4");

        // 1. Software fallback ("none") with H.264, unknown preset fallback -> "veryfast", CRF < 18 clamped to "18"
        let sw_h264 =
            build_cold_compaction_ffmpeg_args(src, dst, "h264", "bogus_preset", "none", 5);
        assert!(sw_h264.windows(2).any(|w| w == ["-c:v", "libx264"]));
        assert!(sw_h264.windows(2).any(|w| w == ["-preset", "veryfast"]));
        assert!(sw_h264.windows(2).any(|w| w == ["-crf", "18"]));
        assert!(!sw_h264.iter().any(|a| a == "hvc1"));

        // 2. VAAPI with HEVC and CRF > 36 clamped to "36"
        let vaapi_hevc = build_cold_compaction_ffmpeg_args(src, dst, "hevc", "fast", "vaapi", 99);
        assert!(vaapi_hevc.windows(2).any(|w| w == ["-hwaccel", "vaapi"]));
        assert!(vaapi_hevc.windows(2).any(|w| w == ["-c:v", "hevc_vaapi"]));
        assert!(vaapi_hevc.windows(2).any(|w| w == ["-qp", "36"]));
        assert!(vaapi_hevc.windows(2).any(|w| w == ["-tag:v", "hvc1"]));

        // 3. QSV with H.265
        let qsv_hevc = build_cold_compaction_ffmpeg_args(src, dst, "h265", "medium", "QSV", 28);
        assert!(qsv_hevc.windows(2).any(|w| w == ["-hwaccel", "qsv"]));
        assert!(qsv_hevc.windows(2).any(|w| w == ["-c:v", "hevc_qsv"]));
        assert!(qsv_hevc.windows(2).any(|w| w == ["-preset", "medium"]));
        assert!(qsv_hevc.windows(2).any(|w| w == ["-global_quality", "28"]));
        assert!(qsv_hevc.windows(2).any(|w| w == ["-tag:v", "hvc1"]));

        // 4. NVENC preset mapping (ultrafast->p1, veryfast->p2, fast->p3, medium->p4, slow->p5)
        for (preset_in, expected_nv) in [
            ("ultrafast", "p1"),
            ("veryfast", "p2"),
            ("fast", "p3"),
            ("medium", "p4"),
            ("slow", "p5"),
        ] {
            let nv_args =
                build_cold_compaction_ffmpeg_args(src, dst, "h264", preset_in, "nvenc", 26);
            assert!(nv_args.windows(2).any(|w| w == ["-hwaccel", "cuda"]));
            assert!(nv_args.windows(2).any(|w| w == ["-c:v", "h264_nvenc"]));
            assert!(nv_args.windows(2).any(|w| w == ["-preset", expected_nv]));
            assert!(nv_args.windows(2).any(|w| w == ["-cq", "26"]));
        }
    }

    #[test]
    fn yuv_frame_y_plane_and_parse_recording_segment_path_handle_edge_cases() {
        // YuvFrame::y_plane normal and truncated buffer clamping
        let full_frame = YuvFrame {
            camera: Arc::from("cam"),
            width: 4,
            height: 4,
            data: Arc::new(vec![42u8; 24]),
            timestamp: 1.0,
        };
        assert_eq!(full_frame.y_plane().len(), 16);

        let truncated_frame = YuvFrame {
            camera: Arc::from("cam"),
            width: 100,
            height: 100,
            data: Arc::new(vec![7u8; 10]),
            timestamp: 1.0,
        };
        assert_eq!(truncated_frame.y_plane().len(), 10);

        // CameraActivityRing record_enabled toggle and 3-tuple aggregate_range wrapper
        let mut ring = CameraActivityRing::default();
        assert!(ring.is_record_enabled());
        ring.set_record_enabled(false);
        assert!(!ring.is_record_enabled());
        ring.set_record_enabled(true);
        ring.record_sample(500.0, 120, 2, 1);
        assert_eq!(ring.aggregate_range(495.0, 505.0), (120, 2, 1));

        // parse_recording_segment_path collision suffix and invalid path rejection
        let collided = Path::new("/media/recordings/2026-11-02/14/front_door/05.10-a1b2.mp4");
        let (cam, ts) =
            parse_recording_segment_path(collided).expect("collision suffix should parse");
        assert_eq!(cam, "front_door");
        assert!(ts > 0.0);

        assert!(
            parse_recording_segment_path(Path::new(
                "/media/recordings/2026-11-02/14/.staging/05.10.mp4"
            ))
            .is_none()
        );
        assert!(
            parse_recording_segment_path(Path::new(
                "/media/recordings/2026-11-02/14/front_door/60.10.mp4"
            ))
            .is_none()
        );
        assert!(
            parse_recording_segment_path(Path::new(
                "/media/recordings/2026-11-02/14/front_door/05.60.mp4"
            ))
            .is_none()
        );

        // build_privacy_drawbox_filter supports object and nested-object privacy_mask configs
        let obj_mask = serde_json::json!({
            "mode": "pixelate",
            "coordinates": "0.1,0.2,0.5,0.2,0.5,0.6,0.1,0.6"
        });
        let f_obj = build_privacy_drawbox_filter(Some(&obj_mask)).expect("object mask filter");
        assert!(f_obj.contains("drawbox=x=iw*0.100:y=ih*0.200:w=iw*0.400:h=ih*0.400"));

        let nested_mask = serde_json::json!({
            "neighbor_window": { "coordinates": "0.2,0.1,0.4,0.1,0.4,0.3,0.2,0.3" }
        });
        let f_nested =
            build_privacy_drawbox_filter(Some(&nested_mask)).expect("nested object mask filter");
        assert!(f_nested.contains("drawbox=x=iw*0.200:y=ih*0.100:w=iw*0.200:h=ih*0.200"));

        // parse_staged_start_time extracts exact UTC epoch from {camera}_%Y%m%d%H%M%S.mp4
        let staged_ok =
            Path::new("/media/recordings/.staging/front_door/front_door_20261102140510.mp4");
        let expected_epoch = chrono::NaiveDate::from_ymd_opt(2026, 11, 2)
            .unwrap()
            .and_hms_opt(14, 5, 10)
            .unwrap()
            .and_utc()
            .timestamp() as f64;
        assert_eq!(
            parse_staged_start_time("front_door", staged_ok),
            Some(expected_epoch)
        );
        // Wrong camera prefix or malformed date returns None
        assert_eq!(parse_staged_start_time("backyard", staged_ok), None);
        assert_eq!(
            parse_staged_start_time(
                "front_door",
                Path::new("/media/recordings/.staging/front_door/front_door_20269999999999.mp4")
            ),
            None
        );
    }

    #[tokio::test]
    async fn build_mp4_export_with_privacy_skips_db_and_thumbnails_for_transient_share_exports() {
        let root = std::env::temp_dir().join(format!(
            "ninjanvr-transient-share-export-{}",
            random_suffix()
        ));
        let rec_dir = root.join("recordings/2026-11-01/12/front_door");
        std::fs::create_dir_all(&rec_dir).unwrap();

        let db_path = root.join("ninjanvr.db");
        let db = NinjaDatabase::open(&db_path).unwrap();

        let seg_disk = rec_dir.join("00.00.mp4");
        let mut fake_mp4 = vec![0u8; 2048];
        fake_mp4[0..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p']);
        std::fs::write(&seg_disk, &fake_mp4).unwrap();

        db.insert_recording(&RecordingRow {
            id: "seg_share_1".to_string(),
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

        for transient_id in ["share_redacted_ev_1", "share_clip_ev_2"] {
            build_mp4_export_with_privacy(
                &db,
                &root,
                transient_id,
                "front_door",
                "Transient Share Clip",
                1000.0,
                1010.0,
                false,
                None,
            )
            .await
            .unwrap();

            let final_mp4 = root.join("exports").join(format!("{transient_id}.mp4"));
            let thumb_webp = root.join("clips").join(format!("{transient_id}.webp"));
            assert!(
                final_mp4.exists(),
                "transient share MP4 must exist for {transient_id}"
            );
            assert!(
                !thumb_webp.exists(),
                "transient share export must not create .webp thumbnail for {transient_id}"
            );
            assert!(
                db.get_export_by_id(transient_id).unwrap().is_none(),
                "transient share export must not insert row into export table for {transient_id}"
            );
        }

        assert!(db.query_exports().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn mp4_file_has_audio_track_and_export_audio_strip_fail_closed_prevent_audio_leak() {
        fn make_box(btype: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let sz = (8 + body.len()) as u32;
            let mut out = Vec::with_capacity(sz as usize);
            out.extend_from_slice(&sz.to_be_bytes());
            out.extend_from_slice(btype);
            out.extend_from_slice(body);
            out
        }

        let build_soun_mp4 = |sample_count: u32, sample_size: u32| -> Vec<u8> {
            let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso2mp41");
            let mdat_payload = vec![0x2Au8; (sample_count * sample_size).max(64) as usize];
            let mdat_offset = (ftyp.len() + 8) as u32;
            let mdat = make_box(b"mdat", &mdat_payload);

            let mut hdlr_body = vec![0u8; 24];
            hdlr_body[8..12].copy_from_slice(b"soun");
            let hdlr = make_box(b"hdlr", &hdlr_body);

            let mut stsz_body = Vec::new();
            stsz_body.extend_from_slice(&0u32.to_be_bytes());
            stsz_body.extend_from_slice(&sample_size.to_be_bytes());
            stsz_body.extend_from_slice(&sample_count.to_be_bytes());
            let stsz = make_box(b"stsz", &stsz_body);

            let mut stco_body = Vec::new();
            stco_body.extend_from_slice(&0u32.to_be_bytes());
            stco_body.extend_from_slice(&1u32.to_be_bytes());
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

        let root = std::env::temp_dir().join(format!(
            "ninjanvr-audio-strip-fail-closed-{}",
            random_suffix()
        ));
        let rec_dir = root.join("recordings/2026-11-04/12/front_door");
        std::fs::create_dir_all(&rec_dir).unwrap();

        let video_only_path = root.join("video_only.mp4");
        let mut fake_video_mp4 = vec![0u8; 2048];
        fake_video_mp4[0..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p']);
        std::fs::write(&video_only_path, &fake_video_mp4).unwrap();

        let empty_soun_path = root.join("empty_soun.mp4");
        std::fs::write(&empty_soun_path, build_soun_mp4(0, 0)).unwrap();

        let minimal_soun_trak_path = root.join("minimal_soun_trak.mp4");
        let mut hdlr_body = vec![0u8; 24];
        hdlr_body[8..12].copy_from_slice(b"soun");
        let min_moov = make_box(
            b"moov",
            &make_box(b"trak", &make_box(b"mdia", &make_box(b"hdlr", &hdlr_body))),
        );
        std::fs::write(&minimal_soun_trak_path, &min_moov).unwrap();

        let seg_with_audio = rec_dir.join("00.00.mp4");
        std::fs::write(&seg_with_audio, build_soun_mp4(256, 2)).unwrap();

        assert!(!mp4_file_has_audio_track(&root.join("missing.mp4")));
        assert!(!mp4_file_has_audio_track(&video_only_path));
        assert!(!mp4_file_has_audio_track(&empty_soun_path));
        assert!(mp4_file_has_audio_track(&minimal_soun_trak_path));
        assert!(mp4_file_has_audio_track(&seg_with_audio));

        // moov > 256 KB (e.g. 300 KB free padding before trak):
        // - video-only moov > 256 KB returns false
        // - audio-bearing moov > 256 KB (where soun trak sits past 256 KB offset) returns true
        let padding_300kb = make_box(b"free", &vec![0u8; 300 * 1024]);
        let mut vide_hdlr_body = vec![0u8; 24];
        vide_hdlr_body[8..12].copy_from_slice(b"vide");
        let vide_trak = make_box(
            b"trak",
            &make_box(b"mdia", &make_box(b"hdlr", &vide_hdlr_body)),
        );
        let soun_trak = make_box(b"trak", &make_box(b"mdia", &make_box(b"hdlr", &hdlr_body)));

        let mut large_video_moov_body = Vec::with_capacity(padding_300kb.len() + vide_trak.len());
        large_video_moov_body.extend_from_slice(&padding_300kb);
        large_video_moov_body.extend_from_slice(&vide_trak);
        let large_video_only_path = root.join("large_moov_video_only.mp4");
        std::fs::write(
            &large_video_only_path,
            make_box(b"moov", &large_video_moov_body),
        )
        .unwrap();
        assert!(!mp4_file_has_audio_track(&large_video_only_path));

        let mut large_audio_moov_body = Vec::with_capacity(padding_300kb.len() + soun_trak.len());
        large_audio_moov_body.extend_from_slice(&padding_300kb);
        large_audio_moov_body.extend_from_slice(&soun_trak);
        let large_audio_path = root.join("large_moov_with_audio.mp4");
        std::fs::write(&large_audio_path, make_box(b"moov", &large_audio_moov_body)).unwrap();
        assert!(mp4_file_has_audio_track(&large_audio_path));

        // moov > 16 MB fails closed (returns true)
        let oversized_moov_path = root.join("oversized_moov_gt_16mb.mp4");
        let oversized_box_size: u32 = (16 * 1024 * 1024 + 8 + 1) as u32;
        let mut oversized_hdr = Vec::with_capacity(16);
        oversized_hdr.extend_from_slice(&oversized_box_size.to_be_bytes());
        oversized_hdr.extend_from_slice(b"moov");
        oversized_hdr.extend_from_slice(&[0u8; 8]);
        std::fs::write(&oversized_moov_path, &oversized_hdr).unwrap();
        assert!(mp4_file_has_audio_track(&oversized_moov_path));

        // Truncated/unreadable moov body fails closed (returns true)
        let truncated_moov_path = root.join("truncated_moov.mp4");
        let mut truncated_moov = Vec::with_capacity(16);
        truncated_moov.extend_from_slice(&1024u32.to_be_bytes());
        truncated_moov.extend_from_slice(b"moov");
        truncated_moov.extend_from_slice(&[0u8; 8]);
        std::fs::write(&truncated_moov_path, &truncated_moov).unwrap();
        assert!(mp4_file_has_audio_track(&truncated_moov_path));

        let db = NinjaDatabase::open(root.join("ninjanvr.db")).unwrap();
        db.insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_audio_strip_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-04/12/front_door/00.00.mp4".to_string(),
            start_time: 1000.0,
            end_time: 1010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: -18,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

        let err_strip = build_mp4_export_with_privacy(
            &db,
            &root,
            "share_clip_ev_audio_leak",
            "front_door",
            "Audio Stripped Share",
            1000.0,
            1010.0,
            false,
            None,
        )
        .await
        .expect_err("failed ffmpeg audio strip on segment with soun track must fail closed");
        assert!(
            err_strip.to_string().contains(
                "failed to strip audio track for export share_clip_ev_audio_leak on camera front_door"
            ),
            "unexpected error: {err_strip}"
        );
        assert!(
            !root.join("exports/share_clip_ev_audio_leak.mp4").exists(),
            "unstripped audio segment must never be copied to output file"
        );
        assert!(
            db.get_export_by_id("share_clip_ev_audio_leak")
                .unwrap()
                .is_none()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn retention_cleanup_enforces_canonical_media_dir_containment_and_honors_touched_share_links()
     {
        let root = std::env::temp_dir().join(format!(
            "ninjanvr-retention-containment-{}",
            random_suffix()
        ));
        let media_dir = root.join("media");
        let outside_dir = root.join("outside");
        std::fs::create_dir_all(media_dir.join("recordings")).unwrap();
        std::fs::create_dir_all(media_dir.join("exports")).unwrap();
        std::fs::create_dir_all(media_dir.join("clips")).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();

        let db = NinjaDatabase::open(root.join("ninjanvr.db")).unwrap();
        let cfg = Arc::new(RwLock::new(
            NinjaConfig::from_yaml_str(
                r#"
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record]
    record:
      enabled: true
      alerts:
        retain:
          days: 1
      detections:
        retain:
          days: 1
"#,
                root.join("config.yml"),
            )
            .unwrap(),
        ));
        let engine = Arc::new(RecordingEngine::new(
            cfg,
            db.clone(),
            media_dir.clone(),
            new_activity_ledger(),
        ));

        let now = unix_now_f64();

        // 1. Valid expired event inside media_dir -> its artifacts inside media_dir must be removed
        let valid_clip_jpg = media_dir.join("clips/ev_exp_valid.jpg");
        let valid_share_mp4 = media_dir.join("exports/share_redacted_ev_exp_valid.mp4");
        std::fs::write(&valid_clip_jpg, b"jpg").unwrap();
        std::fs::write(&valid_share_mp4, b"mp4").unwrap();
        db.upsert_event(&EventDto {
            id: "ev_exp_valid".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: serde_json::json!([]),
            start_time: 100.0,
            end_time: Some(110.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.9),
            false_positive: Some(false),
            box_coords: None,
            data: serde_json::json!({"max_severity": "alert"}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.9),
            stationary: false,
        })
        .unwrap();

        // 2. Expired review segment whose thumb_path points outside media_dir (and a preview with .. traversal)
        let outside_thumb = outside_dir.join("outside_thumb.webp");
        let outside_preview = outside_dir.join("outside_preview.mp4");
        let outside_symlink_target = outside_dir.join("symlink_target.webp");
        std::fs::write(&outside_thumb, b"keep_outside_thumb").unwrap();
        std::fs::write(&outside_preview, b"keep_outside_preview").unwrap();
        std::fs::write(&outside_symlink_target, b"keep_symlink_target").unwrap();

        let symlink_in_media = media_dir.join("clips/escape_link.webp");
        std::os::unix::fs::symlink(&outside_symlink_target, &symlink_in_media).unwrap();

        db.insert_review_segment(&ReviewItemDto {
            id: "rev_outside_thumb".to_string(),
            camera: "front_door".to_string(),
            start_time: 100.0,
            end_time: Some(110.0),
            severity: "alert".to_string(),
            thumb_path: outside_thumb.to_string_lossy().to_string(),
            data: serde_json::json!({}),
            has_been_reviewed: false,
        })
        .unwrap();
        db.insert_review_segment(&ReviewItemDto {
            id: "rev_symlink_thumb".to_string(),
            camera: "front_door".to_string(),
            start_time: 100.0,
            end_time: Some(110.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/escape_link.webp".to_string(),
            data: serde_json::json!({}),
            has_been_reviewed: false,
        })
        .unwrap();
        db.insert_preview(
            "prev_traversal",
            "front_door",
            "/media/ninjanvr/clips/../../outside/outside_preview.mp4",
            100.0,
            110.0,
            10.0,
        )
        .unwrap();

        // 3. Exhausted max_views = 1 share links:
        //    - target_touched: initial access > 900s ago, but touched via touch_share_link_last_accessed 60s ago -> protected
        //    - target_stale: initial access > 900s ago, never touched -> artifacts removed
        let touched_artifact = media_dir.join("exports/share_clip_target_touched.mp4");
        let stale_artifact = media_dir.join("exports/share_clip_target_stale.mp4");
        std::fs::write(&touched_artifact, b"touched_stream").unwrap();
        std::fs::write(&stale_artifact, b"stale_stream").unwrap();

        db.insert_share_link_with_options(
            "sl_touched",
            "review",
            "target_touched",
            Some("front_door"),
            "admin",
            now - 3600.0,
            now + 3600.0,
            Some(1),
        )
        .unwrap();
        assert!(
            db.record_share_link_access("sl_touched", now - 1200.0)
                .unwrap()
        );
        assert!(
            db.touch_share_link_last_accessed("sl_touched", now - 60.0)
                .unwrap()
        );

        db.insert_share_link_with_options(
            "sl_stale",
            "review",
            "target_stale",
            Some("front_door"),
            "admin",
            now - 3600.0,
            now + 3600.0,
            Some(1),
        )
        .unwrap();
        assert!(
            db.record_share_link_access("sl_stale", now - 1200.0)
                .unwrap()
        );

        // Run retention cleanup artifact removal and share link pruning
        let clip_paths = db
            .cleanup_expired_events_and_reviews("front_door", 2000.0, 2000.0)
            .unwrap();
        for cp in &clip_paths {
            let _ = engine.remove_contained_media_file(cp);
        }
        // Directory paths inside media_dir are also rejected by is_file()
        assert!(!engine.remove_contained_media_file("/media/ninjanvr/clips"));

        engine.prune_expired_share_link_artifacts(now);

        // Contained expired artifacts were removed
        assert!(!valid_clip_jpg.exists());
        assert!(!valid_share_mp4.exists());

        // Outside files and symlink targets outside media_dir were never unlinked
        assert!(outside_thumb.exists());
        assert!(outside_preview.exists());
        assert!(outside_symlink_target.exists());

        // Exhausted share link touched recently is preserved, while stale exhausted link is cleaned up
        assert!(
            touched_artifact.exists(),
            "recently touched exhausted share link artifact must be preserved"
        );
        assert!(
            !stale_artifact.exists(),
            "stale exhausted share link artifact must be removed"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn largesize_u64_overflow_and_extended_boxes_do_not_panic_or_bypass_audio_detection() {
        fn make_box32(btype: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let sz = (8 + body.len()) as u32;
            let mut out = Vec::with_capacity(sz as usize);
            out.extend_from_slice(&sz.to_be_bytes());
            out.extend_from_slice(btype);
            out.extend_from_slice(body);
            out
        }

        fn make_box64(btype: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let sz = (16 + body.len()) as u64;
            let mut out = Vec::with_capacity(sz as usize);
            out.extend_from_slice(&1u32.to_be_bytes());
            out.extend_from_slice(btype);
            out.extend_from_slice(&sz.to_be_bytes());
            out.extend_from_slice(body);
            out
        }

        let root =
            std::env::temp_dir().join(format!("ninjanvr-largesize-test-{}", random_suffix()));
        std::fs::create_dir_all(&root).unwrap();

        // 1. Crafted MP4 with ftyp (24 bytes) followed by a 64-bit largesize box with size = u64::MAX - 23
        //    (which previously panicked with integer overflow in debug or wrapped pos to 0 in release)
        let ftyp = make_box32(b"ftyp", b"isom\x00\x00\x02\x00isom");
        let mut wrap_mp4 = ftyp.clone();
        wrap_mp4.extend_from_slice(&1u32.to_be_bytes());
        wrap_mp4.extend_from_slice(b"free");
        wrap_mp4.extend_from_slice(&(u64::MAX - (ftyp.len() as u64) + 1).to_be_bytes());
        wrap_mp4.extend_from_slice(&[0u8; 16]);
        let wrap_path = root.join("wrap_overflow.mp4");
        std::fs::write(&wrap_path, &wrap_mp4).unwrap();

        assert_eq!(probe_mp4_duration_fast(&wrap_path), None);
        assert_eq!(probe_mp4_audio_dbfs_fast(&wrap_path), 0);
        assert!(
            mp4_file_has_audio_track(&wrap_path),
            "corrupted u64 overflow box header must fail closed in mp4_file_has_audio_track"
        );

        // 2. MP4 where inner trak/mdia/hdlr boxes use valid 64-bit extended size headers (size32 == 1)
        let mut hdlr_soun = vec![0u8; 24];
        hdlr_soun[8..12].copy_from_slice(b"soun");
        let soun_trak_64 = make_box64(
            b"trak",
            &make_box64(b"mdia", &make_box64(b"hdlr", &hdlr_soun)),
        );
        let mut mp4_64_soun = ftyp.clone();
        mp4_64_soun.extend_from_slice(&make_box64(b"moov", &soun_trak_64));
        let soun64_path = root.join("soun_64bit_boxes.mp4");
        std::fs::write(&soun64_path, &mp4_64_soun).unwrap();
        assert!(
            mp4_file_has_audio_track(&soun64_path),
            "64-bit extended inner boxes must not bypass audio track detection"
        );

        // 3. Multi-soun-track MP4 where first soun track has sample_count = 0 and second soun track has active audio
        let build_soun_trak_with_samples = |count: u32, sz: u32| -> Vec<u8> {
            let mut stsz_body = Vec::new();
            stsz_body.extend_from_slice(&0u32.to_be_bytes());
            stsz_body.extend_from_slice(&sz.to_be_bytes());
            stsz_body.extend_from_slice(&count.to_be_bytes());
            let stbl = make_box32(b"stbl", &make_box32(b"stsz", &stsz_body));
            let minf = make_box32(b"minf", &stbl);
            let mut mdia_children = make_box32(b"hdlr", &hdlr_soun);
            mdia_children.extend_from_slice(&minf);
            make_box32(b"trak", &make_box32(b"mdia", &mdia_children))
        };
        let mut dual_soun_moov = build_soun_trak_with_samples(0, 0);
        dual_soun_moov.extend_from_slice(&build_soun_trak_with_samples(128, 4));
        let mut dual_soun_mp4 = ftyp;
        dual_soun_mp4.extend_from_slice(&make_box32(b"moov", &dual_soun_moov));
        let dual_soun_path = root.join("dual_soun.mp4");
        std::fs::write(&dual_soun_path, &dual_soun_mp4).unwrap();
        assert!(
            mp4_file_has_audio_track(&dual_soun_path),
            "empty first soun track must not mask active second soun track"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn security_regressions_for_privacy_clamping_concat_injection_and_path_containment() {
        // 1. Overhanging normalized privacy mask coordinates must clamp to [0.0, 1.0] instead of emitting 0x0 pixel drawbox
        let overhang_norm = json!(["-0.02,0.20,1.02,0.20,1.02,0.80,-0.02,0.80"]);
        let vf_overhang = build_privacy_drawbox_filter(Some(&overhang_norm))
            .expect("overhanging normalized polygon must clamp cleanly");
        assert_eq!(
            vf_overhang,
            "drawbox=x=iw*0.000:y=ih*0.200:w=iw*1.000:h=ih*0.600:color=#0c0e12@1.0:t=fill"
        );

        // 2. Negative pixel overhang must clamp x_min/y_min to 0 before computing width/height
        let neg_pixel = json!("-50,100,300,100,300,400,-50,400");
        let vf_neg_px = build_privacy_drawbox_filter(Some(&neg_pixel))
            .expect("negative pixel overhang must clamp cleanly");
        assert_eq!(
            vf_neg_px,
            "drawbox=x=0:y=100:w=300:h=300:color=#0c0e12@1.0:t=fill"
        );

        // 3. Multi-polygon array with one valid and one malformed polygon must fail closed (return None)
        let partial_corrupt = json!(["0.1,0.1,0.5,0.5", "corrupt_zone"]);
        assert_eq!(build_privacy_drawbox_filter(Some(&partial_corrupt)), None);
        assert!(privacy_mask_is_configured(Some(&json!(true))));
        assert_eq!(build_privacy_drawbox_filter(Some(&json!(true))), None);

        // 4. FFmpeg concat line helper rejects newline / carriage-return / nul directive injection
        assert!(format_ffmpeg_concat_file_line(Path::new("/tmp/safe_seg.mp4")).is_some());
        assert_eq!(
            format_ffmpeg_concat_file_line(Path::new("/tmp/evil\nfile '/etc/passwd'.mp4")),
            None
        );
        assert_eq!(
            format_ffmpeg_concat_file_line(Path::new("/tmp/evil\roption.mp4")),
            None
        );

        // 5. Relative path starting with '-' or protocol ':' is prefixed with './' for FFmpeg CLI safety
        let dash_args = build_mp4_export_ffmpeg_args(
            Path::new("-list.txt"),
            0.0,
            10.0,
            true,
            Path::new("-out.mp4"),
        );
        assert!(dash_args.iter().any(|a| a == "./-list.txt"));
        assert_eq!(dash_args.last().unwrap(), "./-out.mp4");

        // 6. Export, cold migration, and recording retention reject path traversal and symlinks
        let root =
            std::env::temp_dir().join(format!("ninjanvr-sec-containment-{}", random_suffix()));
        let media_dir = root.join("media");
        let cold_dir = root.join("cold");
        let outside_dir = root.join("outside");
        std::fs::create_dir_all(media_dir.join("recordings/2026-11-01/12/front_door")).unwrap();
        std::fs::create_dir_all(media_dir.join("exports")).unwrap();
        std::fs::create_dir_all(media_dir.join("clips")).unwrap();
        std::fs::create_dir_all(&cold_dir).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();

        let outside_victim = outside_dir.join("victim.mp4");
        std::fs::write(&outside_victim, b"DO_NOT_DELETE_OR_MIGRATE").unwrap();

        let db = NinjaDatabase::open(root.join("ninjanvr.db")).unwrap();
        db.insert_recording(&RecordingRow {
            id: "rec_trav_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/../../outside/victim.mp4".to_string(),
            start_time: 100.0,
            end_time: 110.0,
            duration: 10.0,
            motion: 0,
            objects: 0,
            dbfs: 0,
            segment_size: 0.1,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();

        // Hot-to-cold migration must refuse '..' traversal and preserve outside_victim
        let migrated = migrate_hot_segments_to_cold(&db, &media_dir, &cold_dir, 5000.0).unwrap();
        assert_eq!(migrated, 0);
        assert!(outside_victim.exists());

        // RecordingEngine::remove_contained_recording_file must refuse '..' traversal and symlinks
        let cfg = Arc::new(RwLock::new(
            NinjaConfig::from_yaml_str("cameras: {}\n", root.join("config.yml")).unwrap(),
        ));
        let engine =
            RecordingEngine::new(cfg, db.clone(), media_dir.clone(), new_activity_ledger());
        assert!(!engine.remove_contained_recording_file(
            "/media/ninjanvr/recordings/../../outside/victim.mp4",
            Some(&cold_dir),
            &media_dir.join("recordings"),
        ));
        assert!(outside_victim.exists());

        // build_mp4_export_with_privacy must reject traversal in export_id or camera
        assert!(
            build_mp4_export_with_privacy(
                &db,
                &media_dir,
                "../outside/pwn",
                "front_door",
                "Bad Export",
                100.0,
                110.0,
                true,
                None,
            )
            .await
            .is_err()
        );
        assert!(
            backup_alert_clip(
                &db,
                &media_dir,
                &outside_dir,
                "../evil_cam",
                "person",
                100.0,
                110.0,
                None,
                false,
            )
            .await
            .is_err()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn cycle2_media_privacy_mask_fail_closed_symlink_containment_and_large_moov_audio() {
        fn make_box(btype: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let sz = (8 + body.len()) as u32;
            let mut out = Vec::with_capacity(sz as usize);
            out.extend_from_slice(&sz.to_be_bytes());
            out.extend_from_slice(btype);
            out.extend_from_slice(body);
            out
        }

        // 1. MEDIA-C2-01: Out-of-bounds normalized coordinates must fail closed (None), never degrade to 1x1 pixel drawbox
        let out_of_bounds_norm_neg = json!(["-1.2,0.1,0.8,0.1,0.8,0.9,-1.2,0.9"]);
        assert_eq!(
            build_privacy_drawbox_filter(Some(&out_of_bounds_norm_neg)),
            None
        );
        let out_of_bounds_norm_pos = json!(["0.1,0.1,2.1,0.1,2.1,0.9,0.1,0.9"]);
        assert_eq!(
            build_privacy_drawbox_filter(Some(&out_of_bounds_norm_pos)),
            None
        );

        // Named-zone object map with one valid zone and one empty/malformed zone must fail closed (None)
        let partial_named_zones = json!({
            "window": { "coordinates": "0.1,0.1,0.5,0.5" },
            "door": { "coordinates": "" }
        });
        assert_eq!(
            build_privacy_drawbox_filter(Some(&partial_named_zones)),
            None
        );
        let partial_named_array_zones = json!({
            "window": "0.1,0.1,0.5,0.5",
            "door": []
        });
        assert_eq!(
            build_privacy_drawbox_filter(Some(&partial_named_array_zones)),
            None
        );

        // Deeply nested JSON (>16 levels) must fail closed without stack overflow
        let mut deep_mask = json!("0.1,0.1,0.5,0.5");
        for _ in 0..24 {
            deep_mask = json!([deep_mask]);
        }
        assert_eq!(build_privacy_drawbox_filter(Some(&deep_mask)), None);

        // 2. MEDIA-C2-02 & MEDIA-C2-03: Large moov (>256 KB) in probe_mp4_audio_dbfs_fast,
        //    multi-stsz / multi-moov in mp4_file_has_audio_track, symlink rejection, and legacy dbfs repair containment
        let root = std::env::temp_dir().join(format!("ninjanvr-cycle2-media-{}", random_suffix()));
        let media_dir = root.join("media");
        let outside_dir = root.join("outside");
        let cam_dir = media_dir.join("recordings/2026-11-02/14/front_door");
        std::fs::create_dir_all(&cam_dir).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();

        // Build synthetic MP4 with 300 KB free box inside moov before soun trak with active PCM samples
        let pcm = vec![12_000i16; 512];
        let mut mdat_body = Vec::with_capacity(pcm.len() * 2);
        for &s in &pcm {
            mdat_body.extend_from_slice(&s.to_le_bytes());
        }
        let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00isom");
        let mdat_offset = (ftyp.len() + 8) as u32;
        let mdat = make_box(b"mdat", &mdat_body);
        let padding_300kb = make_box(b"free", &vec![0u8; 300 * 1024]);

        let mut hdlr_soun = vec![0u8; 24];
        hdlr_soun[8..12].copy_from_slice(b"soun");
        let hdlr = make_box(b"hdlr", &hdlr_soun);

        let mut stsz_body = Vec::new();
        stsz_body.extend_from_slice(&0u32.to_be_bytes());
        stsz_body.extend_from_slice(&(mdat_body.len() as u32).to_be_bytes());
        stsz_body.extend_from_slice(&1u32.to_be_bytes());
        let stsz = make_box(b"stsz", &stsz_body);

        let mut stco_body = Vec::new();
        stco_body.extend_from_slice(&0u32.to_be_bytes());
        stco_body.extend_from_slice(&1u32.to_be_bytes());
        stco_body.extend_from_slice(&mdat_offset.to_be_bytes());
        let stco = make_box(b"stco", &stco_body);

        let mut stbl_body = Vec::new();
        stbl_body.extend_from_slice(&stsz);
        stbl_body.extend_from_slice(&stco);
        let minf = make_box(b"minf", &make_box(b"stbl", &stbl_body));
        let mut mdia_body = Vec::new();
        mdia_body.extend_from_slice(&hdlr);
        mdia_body.extend_from_slice(&minf);
        let soun_trak = make_box(b"trak", &make_box(b"mdia", &mdia_body));

        let mut large_moov_children = Vec::new();
        large_moov_children.extend_from_slice(&padding_300kb);
        large_moov_children.extend_from_slice(&soun_trak);
        let large_moov = make_box(b"moov", &large_moov_children);

        let mut large_audio_mp4 = Vec::new();
        large_audio_mp4.extend_from_slice(&ftyp);
        large_audio_mp4.extend_from_slice(&mdat);
        large_audio_mp4.extend_from_slice(&large_moov);

        let large_audio_path = cam_dir.join("05.10.mp4");
        std::fs::write(&large_audio_path, &large_audio_mp4).unwrap();
        let probed_db = probe_mp4_audio_dbfs_fast(&large_audio_path);
        assert!(
            (-15..0).contains(&probed_db),
            "probe_mp4_audio_dbfs_fast must parse soun trak past 256 KB moov offset, got {probed_db}"
        );

        // Symlink to valid audio MP4 must be rejected by probers (None / 0 / fail-closed true)
        let symlink_mp4 = root.join("symlink_seg.mp4");
        std::os::unix::fs::symlink(&large_audio_path, &symlink_mp4).unwrap();
        assert_eq!(probe_mp4_duration_fast(&symlink_mp4), None);
        assert_eq!(probe_mp4_audio_dbfs_fast(&symlink_mp4), 0);
        assert!(mp4_file_has_audio_track(&symlink_mp4));

        // Duplicate stsz in soun stbl (first stsz has count=0, second stsz has count=64, size=4) must return true
        let mut empty_stsz_body = Vec::new();
        empty_stsz_body.extend_from_slice(&0u32.to_be_bytes());
        empty_stsz_body.extend_from_slice(&0u32.to_be_bytes());
        empty_stsz_body.extend_from_slice(&0u32.to_be_bytes());
        let empty_stsz = make_box(b"stsz", &empty_stsz_body);
        let mut dual_stsz_stbl = Vec::new();
        dual_stsz_stbl.extend_from_slice(&empty_stsz);
        dual_stsz_stbl.extend_from_slice(&stsz);
        let dual_stsz_minf = make_box(b"minf", &make_box(b"stbl", &dual_stsz_stbl));
        let mut dual_stsz_mdia = Vec::new();
        dual_stsz_mdia.extend_from_slice(&hdlr);
        dual_stsz_mdia.extend_from_slice(&dual_stsz_minf);
        let dual_stsz_mp4 = make_box(
            b"moov",
            &make_box(b"trak", &make_box(b"mdia", &dual_stsz_mdia)),
        );
        let dual_stsz_path = root.join("dual_stsz.mp4");
        std::fs::write(&dual_stsz_path, &dual_stsz_mp4).unwrap();
        assert!(
            mp4_file_has_audio_track(&dual_stsz_path),
            "empty first stsz must not mask active second stsz in soun track"
        );

        // parse_recording_segment_path rejects leading '-' camera identifier
        assert_eq!(
            parse_recording_segment_path(Path::new(
                "/media/recordings/2026-11-02/14/-evil_cam/05.10.mp4"
            )),
            None
        );

        // run_legacy_dbfs_repair_once resets uncontained traversal rows from -5 to 0 without probing outside media_dir
        let outside_mp4 = outside_dir.join("outside_active.mp4");
        std::fs::write(&outside_mp4, &large_audio_mp4).unwrap();
        let db = NinjaDatabase::open(root.join("ninjanvr.db")).unwrap();
        db.insert_recording(&RecordingRow {
            id: "rec_legacy_outside".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/../../outside/outside_active.mp4".to_string(),
            start_time: 100.0,
            end_time: 110.0,
            duration: 10.0,
            motion: 0,
            objects: 0,
            dbfs: -5,
            segment_size: 0.3,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();
        let cfg = Arc::new(RwLock::new(
            NinjaConfig::from_yaml_str("cameras: {}\n", root.join("config.yml")).unwrap(),
        ));
        let engine =
            RecordingEngine::new(cfg, db.clone(), media_dir.clone(), new_activity_ledger());
        engine.run_legacy_dbfs_repair_once().await;
        let repaired_rows = db.query_recordings("front_door", 90.0, 120.0).unwrap();
        assert_eq!(repaired_rows.len(), 1);
        assert_eq!(
            repaired_rows[0].dbfs, 0,
            "traversal path outside media_dir must be reset to 0 without probing outside file"
        );

        // 3. Top-level box corruption before moov, fragmented moof, trailing truncated bytes, and missing hdlr fail closed
        let mut bad_free_before_moov = vec![0, 0, 0, 4, b'f', b'r', b'e', b'e'];
        bad_free_before_moov.extend_from_slice(&large_moov);
        let bad_free_path = root.join("bad_free_before_moov.mp4");
        std::fs::write(&bad_free_path, &bad_free_before_moov).unwrap();
        assert!(mp4_file_has_audio_track(&bad_free_path));

        let moof_only = make_box(b"moof", &[0u8; 16]);
        let moof_path = root.join("moof_fragment.mp4");
        std::fs::write(&moof_path, &moof_only).unwrap();
        assert!(mp4_file_has_audio_track(&moof_path));

        let mut trailing_garbage = ftyp.clone();
        trailing_garbage.extend_from_slice(b"moov");
        let trailing_path = root.join("trailing_4bytes.mp4");
        std::fs::write(&trailing_path, &trailing_garbage).unwrap();
        assert!(mp4_file_has_audio_track(&trailing_path));

        let no_hdlr_mdia = make_box(b"moov", &make_box(b"trak", &make_box(b"mdia", &minf)));
        let no_hdlr_path = root.join("no_hdlr_mdia.mp4");
        std::fs::write(&no_hdlr_path, &no_hdlr_mdia).unwrap();
        assert!(mp4_file_has_audio_track(&no_hdlr_path));

        // 4. Synthetic non-ISO test byte payloads (first 8 bytes do not start with a known ISO-BMFF 4CC) return false
        for (idx, payload) in [
            vec![0x42u8; 4096],
            (0u8..64u8).collect::<Vec<u8>>(),
            b"REDACTED_V1_BYTES".to_vec(),
        ]
        .into_iter()
        .enumerate()
        {
            let non_iso_path = root.join(format!("synthetic_non_iso_{idx}.mp4"));
            std::fs::write(&non_iso_path, &payload).unwrap();
            assert!(!mp4_file_has_audio_track(&non_iso_path));
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sanitize_retention_days_and_headroom_reject_non_finite_and_overflow_bounds() {
        // FLOATEDGE-001: NaN, +Inf, -Inf, 0.0, and negative values in retention days never coerce to 0.002 or 0.1
        for bad_days in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -7.0,
            -0.001,
        ] {
            let cont = sanitize_retention_days(bad_days, 7.0, 0.002);
            assert_eq!(
                cont, 7.0,
                "bad continuous days {bad_days} must fall back to 7.0"
            );
            assert_ne!(cont, 0.002);

            let alerts = sanitize_retention_days(bad_days, 7.0, 0.1);
            assert_eq!(
                alerts, 7.0,
                "bad alerts days {bad_days} must fall back to 7.0"
            );
            assert_ne!(alerts, 0.1);

            let motion = sanitize_retention_days(bad_days, cont, cont);
            assert_eq!(
                motion, 7.0,
                "bad motion days {bad_days} must fall back to continuous"
            );
            assert!(motion.is_finite());
        }

        // Valid finite retention days clamp cleanly between min_days and MAX_RETENTION_DAYS
        assert_eq!(sanitize_retention_days(0.0005, 7.0, 0.002), 0.002);
        assert_eq!(sanitize_retention_days(14.0, 7.0, 0.002), 14.0);
        assert_eq!(
            sanitize_retention_days(1e100, 7.0, 0.002),
            MAX_RETENTION_DAYS
        );

        // FLOATEDGE-001: NaN, +Inf, -Inf, <= 0.0, and overflowing min_free_gb never produce 0.1 MB cutoffs
        for bad_gb in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -10.0,
            f64::MAX,
            f64::MAX / 512.0,
            MAX_STORAGE_QUOTA_GB * 2.0,
        ] {
            assert_eq!(
                compute_headroom_allowed_mb(5_000.0, 10_000.0, bad_gb),
                None,
                "bad min_free_gb {bad_gb} must never produce a 0.1 MB cutoff"
            );
        }
        for bad_quota in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -5.0,
            f64::MAX,
            f64::MAX / 512.0,
        ] {
            assert_eq!(sanitize_quota_gb(Some(bad_quota)), None);
        }
        assert_eq!(
            sanitize_quota_gb(Some(MAX_STORAGE_QUOTA_GB * 5.0)),
            Some(MAX_STORAGE_QUOTA_GB)
        );
        assert_eq!(sanitize_quota_gb(Some(50.0)), Some(50.0));

        // Valid finite deficit computes exact headroom target and returns None when free space is sufficient
        assert_eq!(compute_headroom_allowed_mb(5_000.0, 4_096.0, 2.0), None);
        assert_eq!(
            compute_headroom_allowed_mb(5_000.0, 1_024.0, 2.0),
            Some(3_976.0)
        );
        assert_eq!(compute_headroom_allowed_mb(f64::NAN, 1_024.0, 2.0), None);
        assert_eq!(
            compute_headroom_allowed_mb(5_000.0, f64::INFINITY, 2.0),
            None
        );
    }

    #[test]
    fn activity_ring_rejects_non_finite_caps_max_samples_and_saturates_sums() {
        let mut ring = CameraActivityRing::default();
        ring.record_sample_with_audio(1_000.0, 100, 2, 1, -30);
        assert_eq!(ring.len(), 1);

        // FLOATEDGE-002: NaN, +Inf, -Inf, and negative timestamps are rejected without wiping history or wedging eviction
        for bad_ts in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            ring.record_sample_with_audio(bad_ts, 999, 9, 9, -5);
        }
        assert_eq!(ring.len(), 1);
        assert_eq!(
            ring.aggregate_range_with_audio(990.0, 1_010.0),
            (100, 2, 1, -30)
        );
        assert_eq!(
            ring.aggregate_range_with_audio(f64::NAN, 1_010.0),
            (0, 0, 0, 0)
        );
        assert_eq!(
            ring.aggregate_range_with_audio(1_010.0, 990.0),
            (0, 0, 0, 0)
        );

        // Advancing past 300s evicts the 1_000.0 sample cleanly
        ring.record_sample_with_audio(1_305.0, 50, 1, 1, -20);
        assert_eq!(ring.len(), 1);
        assert_eq!(
            ring.aggregate_range_with_audio(990.0, 1_310.0),
            (50, 1, 1, -20)
        );

        // RESEXHAUST-008: Pushing > MAX_ACTIVITY_SAMPLES within 300s enforces the hard cap
        for i in 0..(MAX_ACTIVITY_SAMPLES + 500) {
            ring.record_sample_with_audio(
                1_305.0 + (i as f64) * 0.01,
                u32::MAX,
                u32::MAX,
                u32::MAX,
                -15,
            );
        }
        assert_eq!(ring.len(), MAX_ACTIVITY_SAMPLES);

        // Saturating addition handles repeated u32::MAX samples without overflow
        let (m, o, r, peak) = ring.aggregate_range_with_audio(1_300.0, 2_000.0);
        assert_eq!(m, (u32::MAX as i64) * (MAX_ACTIVITY_SAMPLES as i64));
        assert_eq!(o, (u32::MAX as i64) * (MAX_ACTIVITY_SAMPLES as i64));
        assert_eq!(r, (u32::MAX as i64) * (MAX_ACTIVITY_SAMPLES as i64));
        assert_eq!(peak, -15);
    }

    #[tokio::test]
    async fn export_cancellation_and_backup_concat_write_failure_clean_up_without_orphans() {
        let root =
            std::env::temp_dir().join(format!("ninjanvr-cancel-export-test-{}", random_suffix()));
        let rec_dir = root.join("recordings/2026-11-05/12/front_door");
        std::fs::create_dir_all(&rec_dir).unwrap();

        let db = NinjaDatabase::open(root.join("ninjanvr.db")).unwrap();
        let seg1 = rec_dir.join("00.00.mp4");
        let seg2 = rec_dir.join("00.10.mp4");
        let mut fake_mp4 = vec![0u8; 2048];
        fake_mp4[0..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p']);
        std::fs::write(&seg1, &fake_mp4).unwrap();
        std::fs::write(&seg2, &fake_mp4).unwrap();

        db.insert_recording(&RecordingRow {
            id: "seg_c1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-05/12/front_door/00.00.mp4".to_string(),
            start_time: 1730808000.0,
            end_time: 1730808010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: 0,
            segment_size: 0.002,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
        db.insert_recording(&RecordingRow {
            id: "seg_c2".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-05/12/front_door/00.10.mp4".to_string(),
            start_time: 1730808010.0,
            end_time: 1730808020.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: 0,
            segment_size: 0.002,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

        // CANCELSAFETY-003: Pre-insert an `in_progress: true` row, advance `build_mp4_export_with_privacy`
        // across successive `.await` points, and drop the future before completion; verify the DB row is
        // deleted and no orphaned `out_file`, `thumb_file`, or `.tmp.*` files remain on disk.
        for (idx, max_polls) in [1usize, 2, 3].into_iter().enumerate() {
            let export_id = format!("front_door_cancel_test_{idx}");
            db.insert_export(&ninjanvr_db::ExportRow {
                id: export_id.clone(),
                camera: "front_door".to_string(),
                name: "Cancelled Export".to_string(),
                date: 1730808000.0,
                video_path: format!("/media/ninjanvr/exports/{export_id}.mp4"),
                thumb_path: format!("/media/ninjanvr/clips/{export_id}.webp"),
                in_progress: true,
            })
            .unwrap();
            assert!(db.get_export_by_id(&export_id).unwrap().is_some());

            let mut completed_early = false;
            {
                let fut = build_mp4_export_with_privacy(
                    &db,
                    &root,
                    &export_id,
                    "front_door",
                    "Cancelled Export",
                    1730808000.0,
                    1730808020.0,
                    true,
                    None,
                );
                tokio::pin!(fut);
                let waker = std::task::Waker::noop();
                let mut cx = std::task::Context::from_waker(waker);
                for p in 0..max_polls {
                    if std::future::Future::poll(fut.as_mut(), &mut cx).is_ready() {
                        completed_early = true;
                        break;
                    }
                    if p + 1 < max_polls {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;

            if !completed_early {
                assert!(
                    db.get_export_by_id(&export_id).unwrap().is_none(),
                    "dropping build_mp4_export_with_privacy (polls={max_polls}) must delete the stuck in_progress DB row"
                );
                assert!(
                    !root
                        .join("exports")
                        .join(format!("{export_id}.mp4"))
                        .exists(),
                    "cancelled export (polls={max_polls}) must never leave an orphaned MP4 file"
                );
                assert!(
                    !root
                        .join("clips")
                        .join(format!("{export_id}.webp"))
                        .exists(),
                    "cancelled export (polls={max_polls}) must never leave an orphaned thumbnail file"
                );
            }
        }
        if let Ok(entries) = std::fs::read_dir(root.join("exports")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                assert!(
                    !name.ends_with(".tmp.mp4"),
                    "leftover temp MP4 file after cancellation: {name}"
                );
            }
        }
        if let Ok(entries) = std::fs::read_dir(root.join("clips")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                assert!(
                    !name.ends_with(".tmp.webp"),
                    "leftover temp WebP file after cancellation: {name}"
                );
            }
        }

        // RESDISC-008: Multi-segment backup_alert_clip propagates concat list write errors instead of truncating
        let backup_dir = root.join("backup_target");
        let dt = Utc.timestamp_opt(1730808000, 0).single().unwrap();
        let day_dir = backup_dir.join(dt.format("%Y-%m-%d").to_string());
        std::fs::create_dir_all(&day_dir).unwrap();
        // Use a 250-char label so `.{stem}_{nonce:016x}_concat.txt` exceeds 255 bytes (ENAMETOOLONG on all filesystems)
        let long_label = "p".repeat(250);

        let backup_err = backup_alert_clip(
            &db,
            &root,
            &backup_dir,
            "front_door",
            &long_label,
            1730808000.0,
            1730808020.0,
            None,
            false,
        )
        .await
        .expect_err("concat list write failure must propagate instead of silently truncating");
        assert!(
            backup_err
                .to_string()
                .contains("failed to write concat list"),
            "unexpected backup error: {backup_err:#}"
        );
        assert!(
            !day_dir
                .join(format!("front_door_1730808000000_{long_label}.mp4"))
                .exists(),
            "truncated single-segment fallback must not be written when concat list write fails"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn phase2_alert_backup_millisecond_stems_media_allowlist_and_cold_migration_hardening() {
        let root =
            std::env::temp_dir().join(format!("ninjanvr-phase2-media-sec-{}", random_suffix()));
        let media_dir = root.join("media");
        let cold_dir = root.join("cold");
        let backup_dir = root.join("backup");
        let rec_dir = media_dir.join("recordings/2026-11-06/12/front_door");
        std::fs::create_dir_all(&rec_dir).unwrap();
        std::fs::create_dir_all(media_dir.join("clips/review")).unwrap();
        std::fs::create_dir_all(media_dir.join("exports")).unwrap();
        std::fs::create_dir_all(media_dir.join("previews")).unwrap();
        std::fs::create_dir_all(media_dir.join("models")).unwrap();
        std::fs::create_dir_all(media_dir.join("faces/alice")).unwrap();
        std::fs::create_dir_all(&cold_dir).unwrap();

        let db = NinjaDatabase::open(root.join("ninjanvr.db")).unwrap();

        // 1. LOSSYFROM-002 & CANCELSAFETY-003: Sub-second start_time disambiguation and temp file cleanup in backup_alert_clip
        let (stem1, out1, list1, tmp1) = build_alert_backup_paths(
            &backup_dir.join("2024-11-05"),
            "front_door",
            "person",
            1_730_808_000.125,
            0x1111_2222_3333_4444,
        );
        let (stem2, out2, list2, tmp2) = build_alert_backup_paths(
            &backup_dir.join("2024-11-05"),
            "front_door",
            "person",
            1_730_808_000.875,
            0x5555_6666_7777_8888,
        );
        assert_eq!(stem1, "front_door_1730808000125_person");
        assert_eq!(stem2, "front_door_1730808000875_person");
        assert_ne!(out1, out2);
        assert_eq!(
            list1.file_name().unwrap().to_str().unwrap(),
            ".front_door_1730808000125_person_1111222233334444_concat.txt"
        );
        assert_eq!(
            tmp1.file_name().unwrap().to_str().unwrap(),
            ".front_door_1730808000125_person_1111222233334444.mp4.tmp"
        );
        assert_ne!(list1, list2);
        assert_ne!(tmp1, tmp2);

        let seg1_path = rec_dir.join("00.00.mp4");
        let seg2_path = rec_dir.join("00.10.mp4");
        std::fs::write(&seg1_path, b"seg_1_payload_bytes").unwrap();
        std::fs::write(&seg2_path, b"seg_2_payload_bytes").unwrap();
        let snap1_path = media_dir.join("clips/review/snap1.webp");
        let snap2_path = media_dir.join("clips/review/snap2.webp");
        std::fs::write(&snap1_path, b"snap_1_webp").unwrap();
        std::fs::write(&snap2_path, b"snap_2_webp").unwrap();

        db.insert_recording(&RecordingRow {
            id: "rec_p2_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-06/12/front_door/00.00.mp4".to_string(),
            start_time: 1_730_808_000.0,
            end_time: 1_730_808_010.0,
            duration: 10.0,
            motion: 10,
            objects: 1,
            dbfs: 0,
            segment_size: 0.01,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
        db.insert_recording(&RecordingRow {
            id: "rec_p2_2".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-06/12/front_door/00.10.mp4".to_string(),
            start_time: 1_730_808_010.0,
            end_time: 1_730_808_020.0,
            duration: 10.0,
            motion: 10,
            objects: 1,
            dbfs: 0,
            segment_size: 0.01,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

        let bak_a = backup_alert_clip(
            &db,
            &media_dir,
            &backup_dir,
            "front_door",
            "person",
            1_730_808_000.125,
            1_730_808_018.0,
            Some("/media/ninjanvr/clips/review/snap1.webp"),
            true,
        )
        .await
        .unwrap();
        let bak_b = backup_alert_clip(
            &db,
            &media_dir,
            &backup_dir,
            "front_door",
            "person",
            1_730_808_000.875,
            1_730_808_018.0,
            Some("/media/ninjanvr/clips/review/snap2.webp"),
            true,
        )
        .await
        .unwrap();
        assert_ne!(
            bak_a, bak_b,
            "same-second sub-second alerts must not overwrite each other"
        );
        assert!(bak_a.exists());
        assert!(bak_b.exists());
        assert_eq!(
            std::fs::read(bak_a.with_extension("webp")).unwrap(),
            b"snap_1_webp"
        );
        assert_eq!(
            std::fs::read(bak_b.with_extension("webp")).unwrap(),
            b"snap_2_webp"
        );
        // Verify no leftover .tmp or _concat.txt files remain in day_dir
        let day_entries: Vec<String> = std::fs::read_dir(bak_a.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            !day_entries
                .iter()
                .any(|n| n.ends_with(".tmp") || n.ends_with("_concat.txt")),
            "all temporary backup files must be cleaned up: {day_entries:?}"
        );

        // 2. STRCMP-002 & TOCTOU-005: remove_contained_media_file allowlist & subdirectory containment
        let cfg = Arc::new(RwLock::new(
            NinjaConfig::from_yaml_str("cameras: {}\n", root.join("config.yml")).unwrap(),
        ));
        let engine =
            RecordingEngine::new(cfg, db.clone(), media_dir.clone(), new_activity_ledger());

        let upper_db = media_dir.join("ninjanvr.DB");
        let upper_yml = media_dir.join("CONFIG.YML");
        let dot_jwt = media_dir.join(".jwt_secret");
        let dot_profile = media_dir.join(".active_profile");
        let model_onnx = media_dir.join("models/model.onnx");
        let face_jpg = media_dir.join("faces/alice/1.jpg");
        for protected in [
            &upper_db,
            &upper_yml,
            &dot_jwt,
            &dot_profile,
            &model_onnx,
            &face_jpg,
        ] {
            std::fs::write(protected, b"PROTECTED_DATA").unwrap();
        }

        // All protected files (uppercase DB/YML, dotfiles, models/, faces/, recordings/) must be refused
        for forbidden_arg in [
            "/media/ninjanvr/ninjanvr.DB",
            "/media/ninjanvr/CONFIG.YML",
            "/media/ninjanvr/.jwt_secret",
            "/media/ninjanvr/.active_profile",
            "/media/ninjanvr/models/model.onnx",
            "/media/ninjanvr/faces/alice/1.jpg",
            "/media/ninjanvr/recordings/2026-11-06/12/front_door/00.00.mp4",
        ] {
            assert!(
                !engine.remove_contained_media_file(forbidden_arg),
                "remove_contained_media_file must refuse {forbidden_arg}"
            );
        }
        for protected in [
            &upper_db,
            &upper_yml,
            &dot_jwt,
            &dot_profile,
            &model_onnx,
            &face_jpg,
            &seg1_path,
        ] {
            assert!(
                protected.exists(),
                "protected file {} must remain intact",
                protected.display()
            );
        }

        // Disallowed extension inside clips/ (e.g. clips/evil.onnx or clips/.dotfile) must also be refused
        let clip_onnx = media_dir.join("clips/evil.onnx");
        let clip_dotfile = media_dir.join("clips/.hidden");
        std::fs::write(&clip_onnx, b"onnx").unwrap();
        std::fs::write(&clip_dotfile, b"hidden").unwrap();
        assert!(!engine.remove_contained_media_file("/media/ninjanvr/clips/evil.onnx"));
        assert!(!engine.remove_contained_media_file("/media/ninjanvr/clips/.hidden"));
        assert!(clip_onnx.exists());
        assert!(clip_dotfile.exists());

        // Valid media artifacts in clips/, exports/, and previews/ (including uppercase extensions and .mask_hash) are removed
        let valid_clip = media_dir.join("clips/review/ev_1.WEBP");
        let valid_export = media_dir.join("exports/exp_1.MP4");
        let valid_mask_hash = media_dir.join("exports/.share_redacted_1.mask_hash");
        let valid_preview = media_dir.join("previews/prev_1.mp4");
        for artifact in [&valid_clip, &valid_export, &valid_mask_hash, &valid_preview] {
            std::fs::write(artifact, b"valid_artifact").unwrap();
        }
        assert!(engine.remove_contained_media_file("/media/ninjanvr/clips/review/ev_1.WEBP"));
        assert!(engine.remove_contained_media_file("/media/ninjanvr/exports/exp_1.MP4"));
        assert!(
            engine
                .remove_contained_media_file("/media/ninjanvr/exports/.share_redacted_1.mask_hash")
        );
        assert!(engine.remove_contained_media_file("/media/ninjanvr/previews/prev_1.mp4"));
        for artifact in [&valid_clip, &valid_export, &valid_mask_hash, &valid_preview] {
            assert!(
                !artifact.exists(),
                "valid artifact {} must be unlinked",
                artifact.display()
            );
        }

        // 3. TOCTOU-004 & LOSSYSTR-002: migrate_hot_segments_to_cold rejects symlinks and non-UTF-8 cold_dir
        #[cfg(unix)]
        {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            let non_utf8_cold = root.join(OsStr::from_bytes(b"cold_bad_\xff"));
            let err_utf8 = migrate_hot_segments_to_cold(&db, &media_dir, &non_utf8_cold, 2e9)
                .expect_err("non-UTF-8 cold_dir must return an error without deleting hot files");
            assert!(err_utf8.to_string().contains("UTF-8"));
            assert!(
                seg1_path.exists(),
                "hot segment must remain intact when cold_dir is non-UTF-8"
            );

            // Symlink in hot recordings dir pointing to protected file must be skipped by migrate_hot_segments_to_cold
            let symlink_seg = rec_dir.join("00.20.mp4");
            std::os::unix::fs::symlink(&upper_db, &symlink_seg).unwrap();
            db.insert_recording(&RecordingRow {
                id: "rec_p2_symlink".to_string(),
                camera: "front_door".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-06/12/front_door/00.20.mp4".to_string(),
                start_time: 1_730_808_020.0,
                end_time: 1_730_808_030.0,
                duration: 10.0,
                motion: 0,
                objects: 0,
                dbfs: 0,
                segment_size: 0.01,
                regions: 0,
                motion_heatmap: None,
            })
            .unwrap();

            let migrated =
                migrate_hot_segments_to_cold(&db, &media_dir, &cold_dir, 2_000_000_000.0).unwrap();
            assert_eq!(
                migrated, 2,
                "only the 2 regular segments should migrate; symlink segment must be skipped"
            );
            assert!(
                !cold_dir.join("2026-11-06/12/front_door/00.20.mp4").exists(),
                "symlink segment must not be copied to cold tier"
            );
            assert!(upper_db.exists());
        }

        // 4. RESDISC-007: build_mp4_export_inner propagates thumbnail rename error and cleans up staging MP4
        let seg3_path = rec_dir.join("00.30.mp4");
        let mut fake_mp4 = vec![0u8; 2048];
        fake_mp4[0..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p']);
        std::fs::write(&seg3_path, &fake_mp4).unwrap();
        db.insert_recording(&RecordingRow {
            id: "rec_p2_3".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-06/12/front_door/00.30.mp4".to_string(),
            start_time: 1_730_808_030.0,
            end_time: 1_730_808_040.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: 0,
            segment_size: 0.002,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

        // Pre-create `clips/exp_thumb_fail.webp` as a non-empty directory so renaming `tmp_thumb_fin` to `thumb_fin` fails
        let blocked_thumb_dir = media_dir.join("clips/exp_thumb_fail.webp");
        std::fs::create_dir_all(&blocked_thumb_dir).unwrap();
        std::fs::write(blocked_thumb_dir.join("keep"), b"x").unwrap();

        let thumb_err = build_mp4_export(
            &db,
            &media_dir,
            "exp_thumb_fail",
            "front_door",
            "Thumb Fail Export",
            1_730_808_030.0,
            1_730_808_040.0,
        )
        .await;
        assert!(
            thumb_err.is_err(),
            "thumbnail rename failure must propagate as Err"
        );
        assert!(
            !media_dir.join("exports/exp_thumb_fail.mp4").exists(),
            "MP4 must not be published when thumbnail rename fails"
        );
        assert!(
            db.get_export_by_id("exp_thumb_fail").unwrap().is_none(),
            "export DB row must not be committed when thumbnail rename fails"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn retention_pass_purges_db_and_orphan_previews_when_preview_quality_is_off() {
        let root = std::env::temp_dir().join(format!(
            "ninjanvr-preview-off-retention-{}",
            random_suffix()
        ));
        let media_dir = root.join("media");
        let sala_prev_dir = media_dir.join("clips/previews/sala");
        let quarto_prev_dir = media_dir.join("clips/previews/quarto");
        std::fs::create_dir_all(&sala_prev_dir).unwrap();
        std::fs::create_dir_all(&quarto_prev_dir).unwrap();

        let db = NinjaDatabase::open(root.join("ninjanvr.db")).unwrap();
        let now = 1_800_000_000.0;

        let sala_tracked_mp4 = sala_prev_dir.join("1799990000-1799993600.mp4");
        let sala_orphan_mp4 = sala_prev_dir.join("1799986400-1799990000.mp4");
        let sala_tmp_concat = sala_prev_dir.join(".prev_sala_1799990000_0001_concat.txt");
        let quarto_tracked_mp4 = quarto_prev_dir.join("1799990000-1799993600.mp4");
        let event_snap = media_dir.join("clips/sala-ev_keep.webp");
        for f in [
            &sala_tracked_mp4,
            &sala_orphan_mp4,
            &sala_tmp_concat,
            &quarto_tracked_mp4,
            &event_snap,
        ] {
            std::fs::write(f, b"preview_bytes").unwrap();
        }

        db.insert_preview(
            "sala_1799990000",
            "sala",
            "/media/ninjanvr/clips/previews/sala/1799990000-1799993600.mp4",
            now - 1800.0,
            now - 100.0,
            1700.0,
        )
        .unwrap();
        db.insert_preview(
            "quarto_1799990000",
            "quarto",
            "/media/ninjanvr/clips/previews/quarto/1799990000-1799993600.mp4",
            now - 1800.0,
            now - 100.0,
            1700.0,
        )
        .unwrap();

        let yaml = r#"
record:
  enabled: true
  expire_interval: 60
  continuous:
    days: 3.0
  motion:
    days: 0.0
  preview:
    quality: off
cameras:
  sala:
    enabled: true
    record:
      enabled: true
      continuous:
        days: 3.0
      motion:
        days: 0.0
      alerts:
        retain:
          days: 7.0
          mode: active_objects
      detections:
        retain:
          days: 7.0
          mode: active_objects
  quarto:
    enabled: true
    record:
      enabled: true
      continuous:
        days: 3.0
      motion:
        days: 0.0
      preview:
        quality: low
      alerts:
        retain:
          days: 14.0
          mode: active_objects
      detections:
        retain:
          days: 14.0
          mode: active_objects
"#;
        let cfg = Arc::new(RwLock::new(
            NinjaConfig::from_yaml_str(yaml, root.join("config.yml")).unwrap(),
        ));
        let engine =
            RecordingEngine::new(cfg, db.clone(), media_dir.clone(), new_activity_ledger());

        engine.run_retention_pass_once(now).await;

        assert!(
            db.query_previews("sala", 0.0, now + 3600.0)
                .unwrap()
                .is_empty(),
            "disabled camera previews must be deleted from SQLite"
        );
        assert!(!sala_tracked_mp4.exists());
        assert!(!sala_orphan_mp4.exists());
        assert!(!sala_tmp_concat.exists());
        assert!(
            !sala_prev_dir.exists(),
            "empty disabled camera preview dir should be removed"
        );

        assert_eq!(
            db.query_previews("quarto", 0.0, now + 3600.0)
                .unwrap()
                .len(),
            1,
            "camera with preview.quality=low must retain active previews"
        );
        assert!(quarto_tracked_mp4.exists());
        assert!(
            event_snap.exists(),
            "event snapshots in clips/ must remain intact"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn export_resolves_cold_tier_segments_reports_progress_and_refuses_corrupt_multi_segment_truncation()
     {
        let root =
            std::env::temp_dir().join(format!("ninjanvr-export-cold-and-moov-{}", random_suffix()));
        let media_dir = root.join("media");
        let cold_dir = root.join("cold");
        let cold_cam_dir = cold_dir.join("2026-11-01/12/front_door");
        std::fs::create_dir_all(media_dir.join("recordings")).unwrap();
        std::fs::create_dir_all(&cold_cam_dir).unwrap();

        let db = NinjaDatabase::open(root.join("ninjanvr.db")).unwrap();

        // 1. Cold-tier synthetic segment resolves cleanly via `cold_dir` and reports progress to 1.0
        let cold_seg = cold_cam_dir.join("00.00.mp4");
        let mut stub_mp4 = vec![0u8; 2048];
        stub_mp4[0..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p']);
        std::fs::write(&cold_seg, &stub_mp4).unwrap();
        db.insert_recording(&RecordingRow {
            id: "cold_seg_1".to_string(),
            camera: "front_door".to_string(),
            path: cold_seg.to_string_lossy().into_owned(),
            start_time: 5000.0,
            end_time: 5010.0,
            duration: 10.0,
            motion: 3,
            objects: 1,
            dbfs: 0,
            segment_size: 0.002,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();

        let progress_vals: Arc<std::sync::Mutex<Vec<f64>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let progress_sink = progress_vals.clone();
        build_mp4_export_with_progress(Mp4ExportRequest {
            db: &db,
            media_dir: &media_dir,
            cold_dir: Some(&cold_dir),
            export_id: "exp_cold_ok",
            camera: "front_door",
            name: "Cold Tier Clip",
            start_time: 5000.0,
            end_time: 5010.0,
            include_audio: true,
            privacy_mask: None,
            cancel_token: None,
            on_progress: Some(Arc::new(move |p| {
                progress_sink.lock().unwrap().push(p);
            })),
        })
        .await
        .expect("cold-tier export must succeed when cold_dir is supplied");

        assert!(media_dir.join("exports/exp_cold_ok.mp4").exists());
        let recorded_p = progress_vals.lock().unwrap().clone();
        assert!(
            recorded_p.last().copied() == Some(1.0),
            "progress callback must end at 1.0, got {recorded_p:?}"
        );

        // 2. Multi-segment files containing a real `moov` box whose video bitstream is invalid
        //    must fail closed instead of silently truncating to `existing_paths[0]` (10s).
        let hot_cam_dir = media_dir.join("recordings/2026-11-01/13/front_door");
        std::fs::create_dir_all(&hot_cam_dir).unwrap();
        let mut corrupt_moov_mp4 = Vec::new();
        // ftyp (16 bytes) + moov (16 bytes) + mdat (2048 bytes of garbage)
        corrupt_moov_mp4.extend_from_slice(&16u32.to_be_bytes());
        corrupt_moov_mp4.extend_from_slice(b"ftypisom\x00\x00\x02\x00");
        corrupt_moov_mp4.extend_from_slice(&16u32.to_be_bytes());
        corrupt_moov_mp4.extend_from_slice(b"moov\x00\x00\x00\x08free");
        corrupt_moov_mp4.extend_from_slice(&2056u32.to_be_bytes());
        corrupt_moov_mp4.extend_from_slice(b"mdat");
        corrupt_moov_mp4.extend_from_slice(&vec![0xFFu8; 2048]);

        let seg_a = hot_cam_dir.join("00.00.mp4");
        let seg_b = hot_cam_dir.join("00.10.mp4");
        std::fs::write(&seg_a, &corrupt_moov_mp4).unwrap();
        std::fs::write(&seg_b, &corrupt_moov_mp4).unwrap();
        assert!(mp4_file_has_moov_box(&seg_a));
        assert!(!mp4_file_has_moov_box(&cold_seg));

        for (id, p, st) in [("m1", &seg_a, 8000.0), ("m2", &seg_b, 8010.0)] {
            db.insert_recording(&RecordingRow {
                id: id.to_string(),
                camera: "front_door".to_string(),
                path: p.to_string_lossy().into_owned(),
                start_time: st,
                end_time: st + 10.0,
                duration: 10.0,
                motion: 1,
                objects: 0,
                dbfs: 0,
                segment_size: 0.002,
                regions: 0,
                motion_heatmap: None,
            })
            .unwrap();
        }

        let err_multi = build_mp4_export_with_progress(Mp4ExportRequest {
            db: &db,
            media_dir: &media_dir,
            cold_dir: Some(&cold_dir),
            export_id: "exp_corrupt_multi",
            camera: "front_door",
            name: "Corrupt Multi",
            start_time: 8000.0,
            end_time: 8020.0,
            include_audio: true,
            privacy_mask: None,
            cancel_token: None,
            on_progress: None,
        })
        .await;
        assert!(
            err_multi.is_err(),
            "corrupt multi-segment moov export must fail closed instead of silently copying segment 0"
        );
        assert!(!media_dir.join("exports/exp_corrupt_multi.mp4").exists());

        // 3. Pre-cancelled `cancel_token` aborts immediately and cleans up `in_progress` row
        db.insert_export(&ninjanvr_db::ExportRow {
            id: "exp_pre_cancel".to_string(),
            camera: "front_door".to_string(),
            name: "Cancelled Clip".to_string(),
            date: 5005.0,
            video_path: "/media/ninjanvr/exports/exp_pre_cancel.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_pre_cancel.webp".to_string(),
            in_progress: true,
        })
        .unwrap();
        let cancel_tok = Arc::new(AtomicBool::new(true));
        let err_cancel = build_mp4_export_with_progress(Mp4ExportRequest {
            db: &db,
            media_dir: &media_dir,
            cold_dir: Some(&cold_dir),
            export_id: "exp_pre_cancel",
            camera: "front_door",
            name: "Cancelled Clip",
            start_time: 5000.0,
            end_time: 5010.0,
            include_audio: true,
            privacy_mask: None,
            cancel_token: Some(cancel_tok),
            on_progress: None,
        })
        .await;
        assert!(err_cancel.is_err());
        assert!(db.get_export_by_id("exp_pre_cancel").unwrap().is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recorder_ffmpeg_hardening_exponential_backoff_and_staged_monotonic_start_parsing() {
        // 1. Disallowed recorder URLs (leading `-`, local file, concat, subfile, pipe, http) fail closed
        for bad_url in [
            "-i /etc/passwd",
            " -i /etc/passwd",
            "file:///etc/passwd",
            "concat:/tmp/a.mp4|/tmp/b.mp4",
            "subfile,,start,0,end,10,,:/etc/passwd",
            "pipe:0",
            "lavfi:testsrc",
            "http://192.0.2.10:8080/live.mp4",
            "rtsp://169.254.169.254:554/live",
        ] {
            assert!(
                build_recorder_ffmpeg_args(bad_url, "/tmp/cam_seg_%09d.mp4", 0).is_err(),
                "expected recorder URL '{bad_url}' to be rejected"
            );
        }

        // Valid rtsps:// with audio_offset_ms != 0 injects -protocol_whitelist before BOTH -i inputs
        let args = build_recorder_ffmpeg_args(
            "rtsps://192.0.2.10:322/stream1",
            "/tmp/cam_1793628310000_seg_%09d.mp4",
            250,
        )
        .unwrap();
        let wl_count = args
            .windows(2)
            .filter(|w| w == &["-protocol_whitelist", "rtsp,rtsps,rtp,tcp,udp,tls,crypto"])
            .count();
        assert_eq!(wl_count, 2);

        // 2. Exponential backoff schedule: 5s -> 10s -> 20s -> 40s -> 60s (capped)
        let d0 = compute_recorder_backoff_delay(0, "front_door");
        let d1 = compute_recorder_backoff_delay(1, "front_door");
        let d2 = compute_recorder_backoff_delay(2, "front_door");
        let d3 = compute_recorder_backoff_delay(3, "front_door");
        let d4 = compute_recorder_backoff_delay(4, "front_door");
        let d9 = compute_recorder_backoff_delay(9, "front_door");
        assert!((5_000..5_500).contains(&d0.as_millis()));
        assert!((10_000..10_500).contains(&d1.as_millis()));
        assert!((20_000..20_500).contains(&d2.as_millis()));
        assert!((40_000..40_500).contains(&d3.as_millis()));
        assert!((60_000..60_500).contains(&d4.as_millis()));
        assert!((60_000..60_500).contains(&d9.as_millis()));

        // 3. Monotonic staged filename start-time parsing (ms epoch + sequence index) & legacy strftime compatibility
        let p_mono = Path::new("/tmp/front_door_1793628310500_seg_000000003.mp4");
        let ts_mono = parse_staged_start_time("front_door", p_mono).unwrap();
        assert!((ts_mono - 1_793_628_340.5).abs() < 1e-3);

        let p_legacy = Path::new("/tmp/front_door_20261102140510.mp4");
        assert!(parse_staged_start_time("front_door", p_legacy).is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn audio_probe_gating_timeout_cap_reconcile_and_graceful_sigterm_shutdown() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("ninjanvr-media-sup-{}", random_suffix()));
        let media_dir = root.join("media");
        std::fs::create_dir_all(&media_dir).unwrap();
        let db = NinjaDatabase::open(root.join("ninjanvr.db")).unwrap();

        // Build a synthetic MP4 with a compressed `mp4a` audio stsd entry + valid video `mvhd`
        let make_box = |btype: &[u8; 4], payload: &[u8]| -> Vec<u8> {
            let mut out = Vec::with_capacity(8 + payload.len());
            out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
            out.extend_from_slice(btype);
            out.extend_from_slice(payload);
            out
        };
        let mut mvhd_payload = vec![0u8; 24];
        mvhd_payload[12..16].copy_from_slice(&1000u32.to_be_bytes());
        mvhd_payload[16..20].copy_from_slice(&10_000u32.to_be_bytes()); // 10.0s
        let mvhd = make_box(b"mvhd", &mvhd_payload);

        let mut hdlr_soun = vec![0u8; 16];
        hdlr_soun[8..12].copy_from_slice(b"soun");
        let mut stsd_payload = vec![0u8; 8];
        stsd_payload[4..8].copy_from_slice(&1u32.to_be_bytes());
        stsd_payload.extend_from_slice(&make_box(b"mp4a", &[0u8; 28]));
        let mut stsz_payload = vec![0u8; 12];
        stsz_payload[4..8].copy_from_slice(&256u32.to_be_bytes());
        stsz_payload[8..12].copy_from_slice(&10u32.to_be_bytes());
        let mut stco_payload = vec![0u8; 12];
        stco_payload[4..8].copy_from_slice(&1u32.to_be_bytes());
        stco_payload[8..12].copy_from_slice(&32u32.to_be_bytes());
        let stbl = make_box(
            b"stbl",
            &[
                make_box(b"stsd", &stsd_payload),
                make_box(b"stsz", &stsz_payload),
                make_box(b"stco", &stco_payload),
            ]
            .concat(),
        );
        let minf = make_box(b"minf", &stbl);
        let mdia = make_box(b"mdia", &[make_box(b"hdlr", &hdlr_soun), minf].concat());
        let trak_soun = make_box(b"trak", &mdia);
        let moov = make_box(b"moov", &[mvhd, trak_soun].concat());
        let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00isom");
        let mdat = make_box(b"mdat", &vec![0x55u8; 2048]);
        let aac_mp4 = [ftyp, mdat, moov].concat();

        let template_mp4 = root.join("template_aac.mp4");
        std::fs::write(&template_mp4, &aac_mp4).unwrap();

        // 1. Audio probe gating: when decode_compressed == false, fake probe script is NEVER invoked!
        let probe_hits = root.join("probe_hits.log");
        let probe_script = root.join("probe_ffmpeg.sh");
        std::fs::write(
            &probe_script,
            format!(
                "#!/bin/sh\necho hit >> \"{}\"\nexit 0\n",
                probe_hits.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&probe_script, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            probe_mp4_audio_dbfs_with_bin(&template_mp4, false, Some(&probe_script)),
            0
        );
        assert!(
            !probe_hits.exists(),
            "compressed audio FFmpeg decode must NOT run when audio_probe_enabled is false"
        );
        let _ = probe_mp4_audio_dbfs_with_bin(&template_mp4, true, Some(&probe_script));
        assert!(
            probe_hits.exists(),
            "compressed audio FFmpeg decode runs when audio_probe_enabled is true"
        );

        // 2. Hanging audio-probe process is killed & reaped within timeout without hanging worker
        let hang_script = root.join("hang_ffmpeg.sh");
        std::fs::write(&hang_script, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&hang_script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let t_hang = std::time::Instant::now();
        let hang_res = decode_audio_dbfs_with_bin_and_timeout(
            &hang_script,
            &template_mp4,
            std::time::Duration::from_millis(120),
        );
        assert_eq!(hang_res, 0);
        assert!(
            t_hang.elapsed() < std::time::Duration::from_secs(2),
            "timed-out audio probe must terminate promptly"
        );

        // 3. Dynamic reconcile() + graceful SIGTERM shutdown() flushing trailing staged segment to SQLite
        let rec_script = root.join("rec_ffmpeg.sh");
        // Last argument to ffmpeg is `safe_out_pattern` (`.../front_door_<epoch>_seg_%09d.mp4`).
        // On SIGTERM, replace `%09d` with `000000000`, copy valid MP4 trailer, and exit 0!
        std::fs::write(
            &rec_script,
            format!(
                "#!/bin/sh\nfor last; do true; done\nout_seg=$(printf '%s' \"$last\" | sed 's/%09d/000000000/')\ntrap 'cp \"{}\" \"$out_seg\"; exit 0' TERM INT\nwhile :; do sleep 0.05; done\n",
                template_mp4.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&rec_script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let yaml = r#"
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record]
    record:
      enabled: true
      continuous:
        days: 3
"#;
        let cfg = NinjaConfig::from_yaml_str(yaml, root.join("config.yml")).unwrap();
        let shared_cfg = Arc::new(RwLock::new(cfg));
        let ledger: SharedActivityLedger = Arc::new(RwLock::new(HashMap::new()));
        let engine = RecordingEngine::new(
            Arc::clone(&shared_cfg),
            db.clone(),
            media_dir.clone(),
            ledger,
        )
        .with_ffmpeg_bin(rec_script);

        engine.reconcile().await;
        assert_eq!(
            engine.active_recorder_cameras().await,
            vec!["front_door".to_string()]
        );

        // Add a second camera dynamically and reconcile
        {
            let mut w = shared_cfg.write().await;
            let mut patio = w.cameras["front_door"].clone();
            patio.ffmpeg.inputs[0].path = "rtsp://127.0.0.1:8554/patio".to_string();
            w.cameras.insert("patio".to_string(), patio);
        }
        engine.reconcile().await;
        assert_eq!(
            engine.active_recorder_cameras().await,
            vec!["front_door".to_string(), "patio".to_string()]
        );

        // Give rec_ffmpeg.sh 120ms to install its SIGTERM trap, then disable `patio` via reconcile
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        {
            let mut w = shared_cfg.write().await;
            w.cameras.get_mut("patio").unwrap().enabled = false;
        }
        engine.reconcile().await;
        assert_eq!(
            engine.active_recorder_cameras().await,
            vec!["front_door".to_string()]
        );
        let patio_recs = db.query_recordings("patio", 0.0, 9_999_999_999.0).unwrap();
        assert_eq!(
            patio_recs.len(),
            1,
            "disabling camera via reconcile() must SIGTERM recorder and commit trailing segment"
        );

        // Graceful engine shutdown commits front_door's trailing segment and leaves 0 active recorders
        engine.shutdown().await;
        assert!(engine.active_recorder_cameras().await.is_empty());
        let fd_recs = db
            .query_recordings("front_door", 0.0, 9_999_999_999.0)
            .unwrap();
        assert_eq!(
            fd_recs.len(),
            1,
            "shutdown() must SIGTERM active recorders and commit closed segment to SQLite"
        );
        assert!(
            !probe_hits.exists() || fd_recs[0].dbfs == 0,
            "audio_probe_enabled=false must keep dbfs=0 without audio decode"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn in_process_symphonia_mp4_audio_decodes_g711_and_aac_without_spawning_ffmpeg() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "ninjanvr-media-symphonia-{}",
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&root).unwrap();

        let poison_marker = root.join("poison_ffmpeg_called.log");
        let poison_ffmpeg = root.join("poison_ffmpeg.sh");
        std::fs::write(
            &poison_ffmpeg,
            format!(
                "#!/bin/sh\necho spawned >> \"{}\"\nexit 1\n",
                poison_marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&poison_ffmpeg, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Build a valid ISO-BMFF MP4 carrying compressed G.711 A-law (`alaw`) samples in pure Rust.
        let make_box = |btype: &[u8; 4], body: &[u8]| -> Vec<u8> {
            let mut out = Vec::with_capacity(8 + body.len());
            out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
            out.extend_from_slice(btype);
            out.extend_from_slice(body);
            out
        };
        let alaw_samples = vec![0x2Au8; 1600]; // Loud non-silent A-law waveform (~0.2s @ 8kHz)
        let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso2mp41");
        let mdat_offset = (ftyp.len() + 8) as u32;
        let mdat = make_box(b"mdat", &alaw_samples);

        let mut mvhd = vec![0u8; 100];
        mvhd[12..16].copy_from_slice(&8000u32.to_be_bytes());
        mvhd[16..20].copy_from_slice(&1600u32.to_be_bytes());
        mvhd[96..100].copy_from_slice(&2u32.to_be_bytes());

        let mut tkhd = vec![0u8; 84];
        tkhd[3] = 0x07; // enabled | in_movie | in_preview
        tkhd[12..16].copy_from_slice(&1u32.to_be_bytes());
        tkhd[20..24].copy_from_slice(&1600u32.to_be_bytes());
        tkhd[36..38].copy_from_slice(&0x0100u16.to_be_bytes());

        let mut mdhd = vec![0u8; 24];
        mdhd[12..16].copy_from_slice(&8000u32.to_be_bytes());
        mdhd[16..20].copy_from_slice(&1600u32.to_be_bytes());

        let mut hdlr = vec![0u8; 25];
        hdlr[8..12].copy_from_slice(b"soun");

        let smhd = vec![0u8; 8];
        let mut dref_payload = vec![0u8; 8];
        dref_payload[7] = 1;
        dref_payload.extend_from_slice(&make_box(b"url ", &[0, 0, 0, 1]));
        let dinf = make_box(b"dinf", &make_box(b"dref", &dref_payload));

        // AudioSampleEntry `alaw`: 6 reserved + 2 data_ref_idx + 8 reserved + 2 channels(1)
        // + 2 sample_size(8) + 4 reserved + 4 sample_rate(8000 << 16)
        let mut alaw_entry = vec![0u8; 28];
        alaw_entry[6..8].copy_from_slice(&1u16.to_be_bytes());
        alaw_entry[16..18].copy_from_slice(&1u16.to_be_bytes());
        alaw_entry[18..20].copy_from_slice(&8u16.to_be_bytes());
        alaw_entry[24..28].copy_from_slice(&(8000u32 << 16).to_be_bytes());
        let mut stsd_payload = vec![0u8; 8];
        stsd_payload[7] = 1;
        stsd_payload.extend_from_slice(&make_box(b"alaw", &alaw_entry));

        let mut stts_payload = vec![0u8; 8];
        stts_payload[7] = 1;
        stts_payload.extend_from_slice(&1600u32.to_be_bytes());
        stts_payload.extend_from_slice(&1u32.to_be_bytes());

        let mut stsc_payload = vec![0u8; 8];
        stsc_payload[7] = 1;
        stsc_payload.extend_from_slice(&1u32.to_be_bytes());
        stsc_payload.extend_from_slice(&1600u32.to_be_bytes());
        stsc_payload.extend_from_slice(&1u32.to_be_bytes());

        let mut stsz_payload = vec![0u8; 12];
        stsz_payload[4..8].copy_from_slice(&1u32.to_be_bytes());
        stsz_payload[8..12].copy_from_slice(&1600u32.to_be_bytes());

        let mut stco_payload = vec![0u8; 8];
        stco_payload[7] = 1;
        stco_payload.extend_from_slice(&mdat_offset.to_be_bytes());

        let stbl = make_box(
            b"stbl",
            &[
                make_box(b"stsd", &stsd_payload),
                make_box(b"stts", &stts_payload),
                make_box(b"stsc", &stsc_payload),
                make_box(b"stsz", &stsz_payload),
                make_box(b"stco", &stco_payload),
            ]
            .concat(),
        );
        let minf = make_box(
            b"minf",
            &[make_box(b"smhd", &smhd), dinf.clone(), stbl].concat(),
        );
        let mdia = make_box(
            b"mdia",
            &[make_box(b"mdhd", &mdhd), make_box(b"hdlr", &hdlr), minf].concat(),
        );
        let trak = make_box(b"trak", &[make_box(b"tkhd", &tkhd), mdia].concat());
        let moov = make_box(b"moov", &[make_box(b"mvhd", &mvhd), trak].concat());

        let alaw_mp4_path = root.join("camera_alaw.mp4");
        std::fs::write(&alaw_mp4_path, [ftyp, mdat, moov.clone()].concat()).unwrap();

        let alaw_db = probe_mp4_audio_dbfs_with_bin(&alaw_mp4_path, true, Some(&poison_ffmpeg));
        assert!(
            (-40..=-1).contains(&alaw_db),
            "in-process Symphonia decoder must measure active dBFS on G.711 alaw MP4, got {alaw_db}"
        );
        assert!(
            !poison_marker.exists(),
            "in-process MP4 audio decode must NEVER spawn ffmpeg subprocess"
        );

        // 2. Interleaved multi-track MP4 (`vide` high-entropy H.264 NALs + 50 chunks of digital-silent `alaw` 0xD5)
        // must read strictly the 160-byte audio chunks (never bleeding into adjacent `vide` bytes) and detect peaks at chunk #42 (t = 8.4s > chunk #8).
        let build_interleaved_alaw_mp4 = |loud_from_chunk: Option<usize>| -> Vec<u8> {
            let num_chunks = 50usize;
            let ftyp_b = make_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso2mp41");
            let mdat_data_start = (ftyp_b.len() + 8) as u32;
            let mut mdat_payload = Vec::new();
            let mut soun_chunk_offsets = Vec::with_capacity(num_chunks);
            let mut vide_chunk_offsets = Vec::with_capacity(num_chunks);

            for c in 0..num_chunks {
                soun_chunk_offsets.push(mdat_data_start + mdat_payload.len() as u32);
                let fill = if loud_from_chunk.is_some_and(|start_c| c >= start_c) {
                    0x2Au8
                } else {
                    0xD5u8 // ITU-T G.711 A-law digital silence (linear -8 / 32768 -> -72 dBFS)
                };
                mdat_payload.extend(std::iter::repeat_n(fill, 160));

                vide_chunk_offsets.push(mdat_data_start + mdat_payload.len() as u32);
                // High-entropy pseudo-H.264 bitstream immediately following every 160-byte audio chunk
                for k in 0..2048usize {
                    mdat_payload.push(((k.wrapping_mul(73).wrapping_add(c * 19)) & 0xFF) as u8);
                }
            }
            let mdat_b = make_box(b"mdat", &mdat_payload);

            let mut vide_hdlr = vec![0u8; 25];
            vide_hdlr[8..12].copy_from_slice(b"vide");
            let mut vide_stsz = vec![0u8; 12];
            vide_stsz[4..8].copy_from_slice(&2048u32.to_be_bytes());
            vide_stsz[8..12].copy_from_slice(&(num_chunks as u32).to_be_bytes());
            let mut vide_stsc = vec![0u8; 8];
            vide_stsc[7] = 1;
            vide_stsc.extend_from_slice(&1u32.to_be_bytes());
            vide_stsc.extend_from_slice(&1u32.to_be_bytes());
            vide_stsc.extend_from_slice(&1u32.to_be_bytes());
            let mut vide_stco = vec![0u8; 8];
            vide_stco[4..8].copy_from_slice(&(num_chunks as u32).to_be_bytes());
            for off in &vide_chunk_offsets {
                vide_stco.extend_from_slice(&off.to_be_bytes());
            }
            let vide_trak = make_box(
                b"trak",
                &make_box(
                    b"mdia",
                    &[
                        make_box(b"hdlr", &vide_hdlr),
                        make_box(
                            b"minf",
                            &make_box(
                                b"stbl",
                                &[
                                    make_box(b"stsc", &vide_stsc),
                                    make_box(b"stsz", &vide_stsz),
                                    make_box(b"stco", &vide_stco),
                                ]
                                .concat(),
                            ),
                        ),
                    ]
                    .concat(),
                ),
            );

            let mut soun_stsc = vec![0u8; 8];
            soun_stsc[7] = 1;
            soun_stsc.extend_from_slice(&1u32.to_be_bytes());
            soun_stsc.extend_from_slice(&160u32.to_be_bytes());
            soun_stsc.extend_from_slice(&1u32.to_be_bytes());
            let mut soun_stsz = vec![0u8; 12];
            soun_stsz[4..8].copy_from_slice(&1u32.to_be_bytes());
            soun_stsz[8..12].copy_from_slice(&((num_chunks * 160) as u32).to_be_bytes());
            let mut soun_stco = vec![0u8; 8];
            soun_stco[4..8].copy_from_slice(&(num_chunks as u32).to_be_bytes());
            for off in &soun_chunk_offsets {
                soun_stco.extend_from_slice(&off.to_be_bytes());
            }
            let soun_trak = make_box(
                b"trak",
                &[
                    make_box(b"tkhd", &tkhd),
                    make_box(
                        b"mdia",
                        &[
                            make_box(b"mdhd", &mdhd),
                            make_box(b"hdlr", &hdlr),
                            make_box(
                                b"minf",
                                &[
                                    make_box(b"smhd", &smhd),
                                    dinf.clone(),
                                    make_box(
                                        b"stbl",
                                        &[
                                            make_box(b"stsd", &stsd_payload),
                                            make_box(b"stts", &stts_payload),
                                            make_box(b"stsc", &soun_stsc),
                                            make_box(b"stsz", &soun_stsz),
                                            make_box(b"stco", &soun_stco),
                                        ]
                                        .concat(),
                                    ),
                                ]
                                .concat(),
                            ),
                        ]
                        .concat(),
                    ),
                ]
                .concat(),
            );
            let moov_b = make_box(
                b"moov",
                &[make_box(b"mvhd", &mvhd), vide_trak, soun_trak].concat(),
            );
            [ftyp_b, mdat_b, moov_b].concat()
        };

        let silent_interleaved_path = root.join("interleaved_silent_alaw.mp4");
        std::fs::write(&silent_interleaved_path, build_interleaved_alaw_mp4(None)).unwrap();
        let silent_interleaved_db =
            probe_mp4_audio_dbfs_with_bin(&silent_interleaved_path, true, Some(&poison_ffmpeg));
        assert!(
            silent_interleaved_db == 0 || silent_interleaved_db <= -68,
            "silent G.711 alaw interleaved with H.264 NALs must NOT bleed into video bytes (got {silent_interleaved_db} dBFS)"
        );

        let late_peak_path = root.join("interleaved_late_peak_alaw.mp4");
        std::fs::write(&late_peak_path, build_interleaved_alaw_mp4(Some(40))).unwrap();
        let late_peak_db =
            probe_mp4_audio_dbfs_with_bin(&late_peak_path, true, Some(&poison_ffmpeg));
        assert!(
            (-45..=-1).contains(&late_peak_db),
            "late acoustic event after chunk #40 must be sampled and detected, got {late_peak_db} dBFS"
        );

        // 3. Malicious/corrupt fragmented MP4 (`mvex` + `moof`/`trun` with `sample_count = u32::MAX`)
        // must be rejected by `moov_is_safe_for_in_process_demux` in < 50ms without spinning in `symphonia-format-isomp4`.
        let mut mp4a_entry = vec![0u8; 28];
        mp4a_entry[6..8].copy_from_slice(&1u16.to_be_bytes());
        mp4a_entry[16..18].copy_from_slice(&1u16.to_be_bytes());
        mp4a_entry[18..20].copy_from_slice(&16u16.to_be_bytes());
        mp4a_entry[24..28].copy_from_slice(&(16000u32 << 16).to_be_bytes());
        let mut mp4a_stsd = vec![0u8; 8];
        mp4a_stsd[7] = 1;
        mp4a_stsd.extend_from_slice(&make_box(b"mp4a", &mp4a_entry));
        let mut frag_stsz = vec![0u8; 12];
        frag_stsz[4..8].copy_from_slice(&16u32.to_be_bytes());
        frag_stsz[8..12].copy_from_slice(&1u32.to_be_bytes());
        let frag_trak = make_box(
            b"trak",
            &make_box(
                b"mdia",
                &[
                    make_box(b"hdlr", &hdlr),
                    make_box(
                        b"minf",
                        &make_box(
                            b"stbl",
                            &[make_box(b"stsd", &mp4a_stsd), make_box(b"stsz", &frag_stsz)]
                                .concat(),
                        ),
                    ),
                ]
                .concat(),
            ),
        );
        let mvex = make_box(b"mvex", &[0u8; 16]);
        let frag_moov = make_box(b"moov", &[frag_trak.clone(), mvex].concat());
        let mut trun_body = vec![0u8; 8];
        trun_body[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        let moof = make_box(b"moof", &make_box(b"traf", &make_box(b"trun", &trun_body)));
        let evil_frag_path = root.join("evil_trun_max.mp4");
        std::fs::write(
            &evil_frag_path,
            [
                make_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso6"),
                frag_moov.clone(),
                moof.clone(),
            ]
            .concat(),
        )
        .unwrap();
        let noop_ffmpeg = root.join("noop_ffmpeg.sh");
        std::fs::write(&noop_ffmpeg, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&noop_ffmpeg, std::fs::Permissions::from_mode(0o755)).unwrap();
        let t_frag = std::time::Instant::now();
        let frag_db = probe_mp4_audio_dbfs_with_bin(&evil_frag_path, true, Some(&noop_ffmpeg));
        assert_eq!(frag_db, 0);
        assert!(
            t_frag.elapsed() < std::time::Duration::from_millis(500),
            "fragmented trun u32::MAX container must be rejected immediately without spinning"
        );

        assert!(
            moov_is_safe_for_in_process_demux(&moov[8..]),
            "valid non-fragmented moov payload must be accepted by moov_is_safe_for_in_process_demux"
        );
        let mut trailing_junk_moov = moov[8..].to_vec();
        trailing_junk_moov.extend_from_slice(&[0, 0, 0, 0]);
        assert!(
            !moov_is_safe_for_in_process_demux(&trailing_junk_moov),
            "truncated/trailing-junk moov payload must be rejected"
        );
        assert!(
            !moov_is_safe_for_in_process_demux(&frag_moov[8..]),
            "fragmented moov containing mvex must be rejected"
        );

        let smuggled_two_moov_path = root.join("evil_two_moov_trun_max.mp4");
        let clean_mp4a_moov = make_box(b"moov", &frag_trak);
        std::fs::write(
            &smuggled_two_moov_path,
            [
                make_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso6"),
                clean_mp4a_moov,
                frag_moov,
                moof,
            ]
            .concat(),
        )
        .unwrap();
        let t_smug = std::time::Instant::now();
        let smug_db =
            probe_mp4_audio_dbfs_with_bin(&smuggled_two_moov_path, true, Some(&noop_ffmpeg));
        assert_eq!(smug_db, 0);
        assert!(
            t_smug.elapsed() < std::time::Duration::from_millis(500),
            "second-moov + top-level moof/trun smuggling must be rejected without spinning"
        );

        // If real `ffmpeg` is available in the test environment, also verify real `mp4a` AAC-LC MP4!
        let real_ffmpeg = resolve_ffmpeg_bin();
        let aac_mp4_path = root.join("camera_aac_real.mp4");
        if std::process::Command::new(&real_ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=16000:duration=0.5",
                "-c:a",
                "aac",
                "-b:a",
                "64k",
            ])
            .arg(&aac_mp4_path)
            .status()
            .is_ok_and(|s| s.success())
        {
            let aac_db = probe_mp4_audio_dbfs_with_bin(&aac_mp4_path, true, Some(&poison_ffmpeg));
            assert!(
                (-35..=-1).contains(&aac_db),
                "in-process Symphonia decoder must measure active dBFS on AAC-LC MP4, got {aac_db}"
            );
            assert!(
                !poison_marker.exists(),
                "real AAC-LC MP4 audio decode must NEVER spawn ffmpeg subprocess"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
