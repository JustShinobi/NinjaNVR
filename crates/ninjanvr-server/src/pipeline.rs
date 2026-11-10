use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use ninjanvr_comms::SharedCommsHub;
use ninjanvr_config::{CameraConfig, NinjaConfig, scrub_yaml_credentials};
use ninjanvr_db::{NinjaDatabase, ReviewItemDto};
use ninjanvr_infer::AiDetectorEngine;
use ninjanvr_media::{SharedActivityLedger, YuvFrame, backup_alert_clip, resolve_ffmpeg_bin};
use ninjanvr_track::{ByteTracker, TrackedObject, should_replace_snapshot};
pub use ninjanvr_vision::classify_dominant_color_tag;
use ninjanvr_vision::{
    BoundingBox, CameraRoiStateMonitor, RgbImage, RoiStateTransition, SimdMotionDetector,
    annotate_rgb_image_with_boxes, apply_privacy_masks_from_value, check_frame_tamper,
    compute_inference_regions, crop_yuv420p_to_tensor_nhwc, encode_image_jpeg,
    encode_image_jpeg_with_quality, encode_image_webp, yuv420p_to_rgb_image,
    yuv420p_to_rgb_image_scaled,
};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{RwLock, Semaphore};
use tracing::{debug, info, warn};

pub use crate::track_lifecycle::*;

pub const MIN_DETECT_WIDTH: u32 = 160;
pub const MAX_DETECT_WIDTH: u32 = 3840;
pub const MIN_DETECT_HEIGHT: u32 = 120;
pub const MAX_DETECT_HEIGHT: u32 = 2160;
pub const FRAME_READ_TIMEOUT: Duration = Duration::from_secs(15);
pub const MAX_HIGH_RES_SNAPSHOT_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_HIGH_RES_DECODED_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_CONCURRENT_HIGH_RES_SNAPSHOT_TASKS: usize = 4;
pub const MAX_CONCURRENT_GENAI_VERIFY_TASKS: usize = 4;
pub const MAX_CONCURRENT_ALERT_BACKUP_TASKS: usize = 2;
pub const GENAI_VERIFY_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_PRIVACY_MASK_DEPTH: usize = 16;

pub fn build_snapshot_http_client() -> Option<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()
}

/// Validates that a camera stream URL uses `rtsp://` or `rtsps://`, has a valid non-empty host
/// (and optional valid `1..=65535` port), and contains no whitespace, control characters, backslashes,
/// or leading `-` option flags.
pub fn is_valid_rtsp_stream_url(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty()
        || !trimmed.is_ascii()
        || trimmed.starts_with('-')
        || trimmed
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return false;
    }
    let rest = if trimmed
        .as_bytes()
        .get(..7)
        .is_some_and(|b| b.eq_ignore_ascii_case(b"rtsp://"))
    {
        &trimmed[7..]
    } else if trimmed
        .as_bytes()
        .get(..8)
        .is_some_and(|b| b.eq_ignore_ascii_case(b"rtsps://"))
    {
        &trimmed[8..]
    } else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.starts_with('@') {
        return false;
    }
    let host_after_auth = authority.rsplit('@').next().unwrap_or(authority);
    if host_after_auth.is_empty() || host_after_auth.starts_with(':') {
        return false;
    }
    if let Some(stripped_v6) = host_after_auth.strip_prefix('[') {
        let Some((v6_host, port_rest)) = stripped_v6.split_once(']') else {
            return false;
        };
        if v6_host.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        if !port_rest.is_empty() {
            let Some(port_str) = port_rest.strip_prefix(':') else {
                return false;
            };
            if !port_str.parse::<u16>().is_ok_and(|p| p > 0) {
                return false;
            }
        }
        true
    } else {
        if host_after_auth.contains('[') || host_after_auth.contains(']') {
            return false;
        }
        let host = match host_after_auth.split_once(':') {
            Some((h, port_str)) => {
                if !port_str.parse::<u16>().is_ok_and(|p| p > 0) {
                    return false;
                }
                h
            }
            None => host_after_auth,
        };
        !host.is_empty()
            && host.len() <= 253
            && !host.starts_with('.')
            && !host.ends_with('.')
            && host.split('.').all(|lbl| {
                !lbl.is_empty()
                    && lbl.len() <= 63
                    && !lbl.starts_with('-')
                    && !lbl.ends_with('-')
                    && lbl
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
    }
}

/// Clamps raw detection dimensions to `160..=3840` x `120..=2160`, enforces even (`multiple_of(2)`)
/// YUV420p plane alignment so 2x2 chroma subsampling never panics, and computes the exact byte length.
pub fn sanitize_yuv420p_dimensions(width: u32, height: u32) -> (u32, u32, usize) {
    let w = (width.clamp(MIN_DETECT_WIDTH, MAX_DETECT_WIDTH) & !1).max(MIN_DETECT_WIDTH);
    let h = (height.clamp(MIN_DETECT_HEIGHT, MAX_DETECT_HEIGHT) & !1).max(MIN_DETECT_HEIGHT);
    let byte_len = ((w as usize) * (h as usize) * 3) / 2;
    (w, h, byte_len)
}

/// Builds the hardened FFmpeg argument list for substream YUV420p decoding,
/// returning `None` if `sub_url` is not a valid `rtsp://` or `rtsps://` URL.
pub fn build_vision_decoder_ffmpeg_args(
    sub_url: &str,
    width: u32,
    height: u32,
    fps: u32,
) -> Option<Vec<String>> {
    if !is_valid_rtsp_stream_url(sub_url) {
        return None;
    }
    let (w, h, _) = sanitize_yuv420p_dimensions(width, height);
    let clamped_fps = fps.clamp(1, 30);
    let vf_arg = format!("fps={clamped_fps},scale={w}:{h}");
    let fps_str = clamped_fps.to_string();
    Some(vec![
        "-nostdin".to_string(),
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-probesize".to_string(),
        "32768".to_string(),
        "-analyzeduration".to_string(),
        "500000".to_string(),
        "-threads".to_string(),
        "1".to_string(),
        "-skip_loop_filter:v".to_string(),
        "nokey".to_string(),
        "-flags2:v".to_string(),
        "fast".to_string(),
        "-protocol_whitelist".to_string(),
        "rtsp,rtsps,rtp,tcp,udp,tls,crypto".to_string(),
        "-rtsp_transport".to_string(),
        "tcp".to_string(),
        "-timeout".to_string(),
        "10000000".to_string(),
        "-i".to_string(),
        sub_url.trim().to_string(),
        "-an".to_string(),
        "-r".to_string(),
        fps_str,
        "-vf".to_string(),
        vf_arg,
        "-sws_flags".to_string(),
        "fast_bilinear".to_string(),
        "-threads".to_string(),
        "1".to_string(),
        "-f".to_string(),
        "rawvideo".to_string(),
        "-pix_fmt".to_string(),
        "yuv420p".to_string(),
        "pipe:1".to_string(),
    ])
}

/// Sanitizes a single filename component (`camera`, `trk.id`, `rev.id`) so joining onto
/// `clips/` or `clips/review/` can never traverse directories (`..`, `/`, `\`).
pub fn sanitize_filename_component(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('.');
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.replace("..", "_")
    }
}

fn normalize_coord_string(s: &str, src_w: u32, src_h: u32) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut nums = Vec::new();
    for part in trimmed.split(',') {
        let Ok(v) = part.trim().parse::<f32>() else {
            return String::new();
        };
        if !v.is_finite() || v < 0.0 {
            return String::new();
        }
        nums.push(v);
    }
    if nums.len() < 6 || !nums.len().is_multiple_of(2) {
        return String::new();
    }
    let is_already_normalized = nums.iter().all(|&v| (0.0..=1.0).contains(&v));
    if is_already_normalized {
        return trimmed.to_string();
    }
    let fw = src_w.max(1) as f32;
    let fh = src_h.max(1) as f32;
    let mut parts = Vec::with_capacity(nums.len());
    for (idx, &val) in nums.iter().enumerate() {
        let norm = if idx % 2 == 0 {
            (val / fw).clamp(0.0, 1.0)
        } else {
            (val / fh).clamp(0.0, 1.0)
        };
        parts.push(format!("{norm:.6}"));
    }
    parts.join(",")
}

fn normalize_privacy_mask_inner(mask_val: &Value, src_w: u32, src_h: u32, depth: usize) -> Value {
    if depth > MAX_PRIVACY_MASK_DEPTH {
        return Value::Null;
    }
    match mask_val {
        Value::Null => Value::Null,
        Value::String(s) => Value::String(normalize_coord_string(s, src_w, src_h)),
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|v| normalize_privacy_mask_inner(v, src_w, src_h, depth + 1))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                if k == "mode" || k == "style" || k == "pixelate" || k == "block_size" {
                    out.insert(k.clone(), v.clone());
                } else if let Some(s) = v.as_str() {
                    out.insert(
                        k.clone(),
                        Value::String(normalize_coord_string(s, src_w, src_h)),
                    );
                } else {
                    out.insert(
                        k.clone(),
                        normalize_privacy_mask_inner(v, src_w, src_h, depth + 1),
                    );
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Converts any pixel-space polygon coordinates in `mask_val` into normalized `[0.0..1.0]`
/// coordinates relative to `(src_w, src_h)` so applying the privacy mask to downscaled thumbnails
/// (`180p`, `360p`) or upscaled high-res main-stream frames (`1080p`, `4K`) redacts the exact same area.
pub fn normalize_privacy_mask_for_resolution(mask_val: &Value, src_w: u32, src_h: u32) -> Value {
    normalize_privacy_mask_inner(mask_val, src_w, src_h, 0)
}

/// Atomically writes `bytes` to `target_dir.join(filename)` after verifying that `media_dir`,
/// `target_dir`, and the target file are not symlinks and that `target_dir` resolves inside `media_dir`.
pub fn write_contained_media_file(
    media_dir: &std::path::Path,
    target_dir: &std::path::Path,
    filename: &str,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    use std::io::{Error, ErrorKind};
    if filename.is_empty()
        || filename.starts_with('.')
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
        || filename.contains('\0')
    {
        return Err(Error::new(ErrorKind::InvalidInput, "unsafe media filename"));
    }
    if std::fs::symlink_metadata(media_dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "refusing to write through symlinked media_dir",
        ));
    }
    std::fs::create_dir_all(media_dir)?;
    let Ok(rel) = target_dir.strip_prefix(media_dir) else {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "target directory must reside inside media_dir",
        ));
    };
    let mut cur = media_dir.to_path_buf();
    for comp in rel.components() {
        match comp {
            std::path::Component::Normal(part) => {
                cur.push(part);
                if let Ok(meta) = std::fs::symlink_metadata(&cur)
                    && (meta.file_type().is_symlink() || !meta.file_type().is_dir())
                {
                    return Err(Error::new(
                        ErrorKind::PermissionDenied,
                        "refusing to traverse symlinked or non-directory media subdirectory",
                    ));
                }
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "invalid path component in target media directory",
                ));
            }
        }
    }
    std::fs::create_dir_all(target_dir)?;
    let canon_media = std::fs::canonicalize(media_dir)?;
    let canon_target = std::fs::canonicalize(target_dir)?;
    if !canon_target.starts_with(&canon_media) {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "target directory escapes media_dir",
        ));
    }
    let dst_path = target_dir.join(filename);
    if let Ok(dst_meta) = std::fs::symlink_metadata(&dst_path)
        && (dst_meta.file_type().is_symlink() || !dst_meta.file_type().is_file())
    {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "refusing to overwrite symlink or non-regular file in media_dir",
        ));
    }
    let tmp_name = format!(".{filename}.{}.tmp", random_suffix());
    let tmp_path = target_dir.join(tmp_name);
    if std::fs::symlink_metadata(&tmp_path).is_ok() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(nix::libc::O_NOFOLLOW);
        }
        let mut tmp_file = opts.open(&tmp_path)?;
        if let Err(err) = tmp_file.write_all(bytes) {
            drop(tmp_file);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
    }
    if let Err(err) = std::fs::rename(&tmp_path, &dst_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(dst_path)
}

/// Decodes a high-resolution snapshot from `bytes` with strict header-dimension and memory allocation bounds
/// so compressed decompression bombs cannot trigger excessive heap allocations.
pub fn decode_bounded_high_res_snapshot(bytes: &[u8], min_w: u32, min_h: u32) -> Option<RgbImage> {
    if bytes.is_empty() || bytes.len() > MAX_HIGH_RES_SNAPSHOT_BYTES {
        return None;
    }
    let header_reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let (w, h) = header_reader.into_dimensions().ok()?;
    if w < min_w || h < min_h || w > MAX_DETECT_WIDTH || h > MAX_DETECT_HEIGHT {
        return None;
    }
    let rgb_bytes = (w as u64).checked_mul(h as u64)?.checked_mul(3)?;
    if rgb_bytes > MAX_HIGH_RES_DECODED_BYTES {
        return None;
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DETECT_WIDTH);
    limits.max_image_height = Some(MAX_DETECT_HEIGHT);
    limits.max_alloc = Some(MAX_HIGH_RES_DECODED_BYTES);
    reader.limits(limits);
    Some(reader.decode().ok()?.to_rgb8())
}

/// Crops `region` from an already-redacted full-frame `RgbImage` and scales it to `max_height`.
pub fn crop_and_scale_rgb_image(
    full_img: &RgbImage,
    region: BoundingBox,
    max_height: Option<u32>,
) -> RgbImage {
    if full_img.width() == 0 || full_img.height() == 0 {
        return RgbImage::new(1, 1);
    }
    let fw = full_img.width().max(1);
    let fh = full_img.height().max(1);
    let x0 = region.x_min.min(fw - 1);
    let y0 = region.y_min.min(fh - 1);
    let x1 = region.x_max.clamp(x0 + 1, fw);
    let y1 = region.y_max.clamp(y0 + 1, fh);
    let rw = (x1 - x0).max(1);
    let rh = (y1 - y0).max(1);
    let (out_w, out_h) = match max_height {
        Some(mh) if rh > mh && mh > 0 => {
            let ow = ((rw as u64 * mh as u64) / (rh as u64)).max(1) as u32;
            (ow, mh)
        }
        _ => (rw, rh),
    };
    let mut out = RgbImage::new(out_w, out_h);
    for dy in 0..out_h {
        let sy = (y0 + ((dy as u64 * rh as u64) / (out_h as u64)) as u32).min(fh - 1);
        for dx in 0..out_w {
            let sx = (x0 + ((dx as u64 * rw as u64) / (out_w as u64)) as u32).min(fw - 1);
            out.put_pixel(dx, dy, *full_img.get_pixel(sx, sy));
        }
    }
    out
}

/// Crops `region` from `full_img` and scales it into a square `side x side` `RgbImage`
/// (e.g. `112x112` for ArcFace/AdaFace and `decode_or_extract_rgb_grid_16x16` face matching).
pub fn crop_and_scale_rgb_square(full_img: &RgbImage, region: BoundingBox, side: u32) -> RgbImage {
    let eff_side = side.max(16);
    if full_img.width() == 0 || full_img.height() == 0 {
        return RgbImage::new(eff_side, eff_side);
    }
    let fw = full_img.width().max(1);
    let fh = full_img.height().max(1);
    let x0 = region.x_min.min(fw - 1);
    let y0 = region.y_min.min(fh - 1);
    let x1 = region.x_max.clamp(x0 + 1, fw);
    let y1 = region.y_max.clamp(y0 + 1, fh);
    let rw = (x1 - x0).max(1);
    let rh = (y1 - y0).max(1);
    let mut out = RgbImage::new(eff_side, eff_side);
    for dy in 0..eff_side {
        let sy = (y0 + ((dy as u64 * rh as u64) / (eff_side as u64)) as u32).min(fh - 1);
        for dx in 0..eff_side {
            let sx = (x0 + ((dx as u64 * rw as u64) / (eff_side as u64)) as u32).min(fw - 1);
            out.put_pixel(dx, dy, *full_img.get_pixel(sx, sy));
        }
    }
    out
}

/// Validates and normalizes a `YuvFrame` so its width and height are non-zero, bounded, and even
/// (`multiple_of(2)`), repacking planes if odd dimensions were supplied so downstream 4:2:0
/// chroma indexing can never panic.
pub fn normalize_yuv_frame_for_processing(frame: &YuvFrame) -> Option<YuvFrame> {
    if frame.width < 2
        || frame.height < 2
        || frame.width > MAX_DETECT_WIDTH
        || frame.height > MAX_DETECT_HEIGHT
    {
        return None;
    }
    let raw_w = frame.width as usize;
    let raw_h = frame.height as usize;
    let raw_y = raw_w.checked_mul(raw_h)?;
    let raw_uv = raw_y / 4;
    let raw_needed = raw_y.checked_add(raw_uv * 2)?;
    if frame.data.len() < raw_needed {
        return None;
    }
    let even_w = (frame.width & !1) as usize;
    let even_h = (frame.height & !1) as usize;
    if even_w < 2 || even_h < 2 {
        return None;
    }
    if even_w == raw_w && even_h == raw_h {
        return Some(frame.clone());
    }
    let even_y = even_w * even_h;
    let even_uv = even_y / 4;
    let mut repacked = vec![128u8; even_y + even_uv * 2];
    let src_y = &frame.data[..raw_y];
    let src_u = &frame.data[raw_y..raw_y + raw_uv];
    let src_v = &frame.data[raw_y + raw_uv..raw_y + raw_uv * 2];
    let raw_uv_stride = (raw_w / 2).max(1);
    let even_uv_stride = even_w / 2;

    for y in 0..even_h {
        let dst_row = y * even_w;
        let src_row = y * raw_w;
        repacked[dst_row..dst_row + even_w].copy_from_slice(&src_y[src_row..src_row + even_w]);
    }
    for uy in 0..(even_h / 2) {
        for ux in 0..even_uv_stride {
            let src_idx = (uy * raw_uv_stride + ux).min(raw_uv.saturating_sub(1));
            let dst_idx = uy * even_uv_stride + ux;
            repacked[even_y + dst_idx] = src_u[src_idx];
            repacked[even_y + even_uv + dst_idx] = src_v[src_idx];
        }
    }
    Some(YuvFrame {
        camera: frame.camera.clone(),
        timestamp: frame.timestamp,
        width: even_w as u32,
        height: even_h as u32,
        data: Arc::new(repacked),
    })
}

#[derive(Debug, Clone, Default)]
pub struct CameraRuntimeMetrics {
    pub camera_fps: f64,
    pub process_fps: f64,
    pub detection_fps: f64,
    pub pid: u32,
    pub last_frame_ts: f64,
    pub night_mode: bool,
}

pub type SharedCameraMetrics = Arc<RwLock<HashMap<String, CameraRuntimeMetrics>>>;
pub type SharedLatestYuvFrames = Arc<RwLock<HashMap<String, YuvFrame>>>;

fn unix_now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn random_suffix() -> String {
    let n: u32 = rand::random();
    format!("{n:06x}")
}

pub struct FrameLifecycleCtx<'a> {
    pub camera: &'a str,
    pub cam_cfg: &'a CameraConfig,
    pub frame: &'a YuvFrame,
    pub now: f64,
}

/// How often each camera vision loop re-reads the shared config to pick up edits saved
/// from the console (zones, tripwires, state ROIs, masks and motion tuning).
const LIVE_VISION_CONFIG_POLL_SEC: f64 = 1.0;

/// Which live-editable parts of a camera's vision config changed between two snapshots.
/// Decoder settings (`detect` resolution/fps, `ffmpeg` inputs) are intentionally excluded:
/// those still need the process to restart.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LiveVisionConfigDelta {
    pub motion: bool,
    pub state_rois: bool,
    pub regions: bool,
    pub lines: bool,
    pub privacy_mask: bool,
}

impl LiveVisionConfigDelta {
    pub fn any(&self) -> bool {
        self.motion || self.state_rois || self.regions || self.lines || self.privacy_mask
    }
}

fn json_differs<T: serde::Serialize>(a: &T, b: &T) -> bool {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(x), Ok(y)) => x != y,
        _ => false,
    }
}

pub fn diff_live_vision_config(
    current: &CameraConfig,
    next: &CameraConfig,
) -> LiveVisionConfigDelta {
    LiveVisionConfigDelta {
        motion: json_differs(&current.motion, &next.motion),
        state_rois: current.state_rois != next.state_rois,
        regions: json_differs(&current.zones, &next.zones)
            || current.objects.mask != next.objects.mask,
        lines: json_differs(&current.lines, &next.lines),
        privacy_mask: current.privacy_mask != next.privacy_mask,
    }
}

/// Copies only the live-editable sections flagged in `delta` from `next` into `current`.
pub fn apply_live_vision_config(
    current: &mut CameraConfig,
    next: &CameraConfig,
    delta: &LiveVisionConfigDelta,
) {
    if delta.motion {
        current.motion = next.motion.clone();
    }
    if delta.state_rois {
        current.state_rois = next.state_rois.clone();
    }
    if delta.regions {
        current.zones = next.zones.clone();
        current.objects.mask = next.objects.mask.clone();
    }
    if delta.lines {
        current.lines = next.lines.clone();
    }
    if delta.privacy_mask {
        current.privacy_mask = next.privacy_mask.clone();
    }
}

/// Attempts an automatic ONVIF `SystemReboot` (or Dahua CGI `magicBox.cgi?action=reboot` fallback)
/// when a camera's RTSP stream stalls repeatedly while its management port (`:2020` / `:80`) is still reachable.
pub async fn try_watchdog_camera_reboot(
    cfg: &NinjaConfig,
    camera: &str,
    camera_driver_runtime: &crate::camera_protocols::CameraDriverRuntime,
) -> bool {
    let Some(cam_cfg) = cfg.cameras.get(camera) else {
        return false;
    };
    let effective_cam_cfg =
        crate::camera_protocols::enrich_camera_config_from_upstream(cfg, camera, cam_cfg);

    let _ = rustls::crypto::ring::default_provider().install_default();
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(4))
        .build()
    else {
        return false;
    };

    camera_driver_runtime
        .reboot_camera(&client, camera, &effective_cam_cfg)
        .await
        .is_ok()
}

pub struct VisionPipeline {
    pub config: Arc<RwLock<NinjaConfig>>,
    pub db: NinjaDatabase,
    pub comms: SharedCommsHub,
    pub detector: Arc<AiDetectorEngine>,
    pub activity_ledger: SharedActivityLedger,
    pub latest_frames: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    pub latest_yuv_frames: SharedLatestYuvFrames,
    pub preview_frames: crate::preview_frames::PreviewFrameStore,
    pub camera_metrics: SharedCameraMetrics,
    pub media_dir: PathBuf,
    pub log_hub: crate::LogBufferHub,
    pub ptz_active_presets: Arc<RwLock<HashMap<String, Option<String>>>>,
    pub camera_driver_runtime: crate::camera_protocols::CameraDriverRuntime,
    pub snapshot_http_client: Option<reqwest::Client>,
    pub high_res_snapshot_semaphore: Arc<Semaphore>,
    pub genai_verify_semaphore: Arc<Semaphore>,
    pub alert_backup_semaphore: Arc<Semaphore>,
}

impl VisionPipeline {
    pub async fn start(self: Arc<Self>) {
        let cameras: Vec<(String, CameraConfig, String)> = {
            let cfg = self.config.read().await;
            cfg.cameras
                .iter()
                .filter(|(_, c)| c.enabled && c.detect.enabled)
                .filter_map(|(name, c)| {
                    let sub_url = cfg
                        .camera_input_for_role(name, "detect")
                        .or_else(|| cfg.camera_input_for_role(name, "record"))?;
                    Some((name.clone(), c.clone(), sub_url))
                })
                .collect()
        };

        for (cam_name, cam_cfg, sub_url) in cameras {
            let p = self.clone();
            tokio::spawn(async move {
                p.run_camera_vision_loop(cam_name, cam_cfg, sub_url).await;
            });
        }
    }

    async fn run_camera_vision_loop(
        &self,
        camera: String,
        mut cam_cfg: CameraConfig,
        sub_url: String,
    ) {
        let (width, height, frame_byte_len) =
            sanitize_yuv420p_dimensions(cam_cfg.detect.width, cam_cfg.detect.height);
        let fps = cam_cfg.detect.fps.clamp(1, 30);

        let Some(ff_args) = build_vision_decoder_ffmpeg_args(&sub_url, width, height, fps) else {
            let scrubbed_url = scrub_yaml_credentials(&sub_url);
            warn!(
                "Refusing to start vision decoder for {camera}: disallowed or invalid RTSP URL ({scrubbed_url})"
            );
            let _ = self.comms.set_camera_availability(&camera, false);
            return;
        };

        let mut motion_detector = SimdMotionDetector::new(width, height, cam_cfg.motion.clone());
        let mut roi_monitor = CameraRoiStateMonitor::new(&cam_cfg.state_rois);
        let mut tracker = ByteTracker::new(&camera, width, height, &cam_cfg);
        let mut preview_sampler = crate::preview_frames::PreviewFrameSampler::default();
        let (model_w, model_h, _) = self.detector.model_dimensions();
        let init_tensor_len = (model_w as usize)
            .saturating_mul(model_h as usize)
            .saturating_mul(3);
        let mut tensor_buf = vec![0u8; init_tensor_len];
        let mut active_review: Option<ActiveReviewState> = None;
        let mut track_snapshots: HashMap<String, TrackPersistSnapshot> = HashMap::new();

        let ffmpeg_bin = resolve_ffmpeg_bin();
        let cam_arc: Arc<str> = Arc::from(camera.as_str());
        let safe_cam_file = sanitize_filename_component(&camera);
        let mut normalized_priv_mask =
            normalize_privacy_mask_for_resolution(&cam_cfg.privacy_mask, width, height);
        let mut last_live_config_check = unix_now_f64();
        let mut consecutive_stall_cycles = 0u32;
        let mut last_watchdog_reboot_ts = 0.0f64;

        loop {
            let mut cmd = Command::new(&ffmpeg_bin);
            cmd.args(&ff_args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);

            let Ok(mut child) = cmd.spawn() else {
                if self.comms.set_camera_availability(&camera, false) {
                    self.log_hub.push_ninjanvr(
                        "WARNING",
                        &format!("ninjanvr.camera.{camera}"),
                        "Camera stream went OFFLINE (decoder spawn failed)",
                    );
                }
                warn!("Substream decoder for {camera} could not start; retrying in 15s");
                tokio::time::sleep(Duration::from_secs(15)).await;
                continue;
            };

            let child_pid = child.id().unwrap_or(0);
            info!(
                "Started substream vision pipeline for {camera} ({}x{} @ {} FPS, pid={child_pid})",
                width, height, fps
            );

            {
                let mut m_guard = self.camera_metrics.write().await;
                let entry = m_guard
                    .entry(camera.clone())
                    .or_insert_with(|| CameraRuntimeMetrics {
                        camera_fps: 0.0,
                        process_fps: 0.0,
                        detection_fps: 0.0,
                        pid: child_pid,
                        last_frame_ts: 0.0,
                        night_mode: false,
                    });
                entry.pid = child_pid;
            }

            let mut received_any_frame = false;
            if let Some(mut stdout) = child.stdout.take() {
                let mut frame_data: Arc<Vec<u8>> = Arc::new(vec![0u8; frame_byte_len]);
                let mut frame_count = 0u64;
                let mut detect_calls = 0u64;
                let mut window_start = unix_now_f64();
                let mut prev_motion_on = false;
                let mut prev_label_counts: BTreeMap<String, usize> = BTreeMap::new();
                for lbl in &cam_cfg.objects.track {
                    prev_label_counts.insert(lbl.clone(), 0);
                }
                let mut prev_total_objs = 0usize;
                let mut prev_night_ir = false;
                let mut consecutive_tamper_frames = 0u32;
                let tamper_threshold_frames = (fps * 4).max(12);

                while tokio::time::timeout(
                    FRAME_READ_TIMEOUT,
                    stdout.read_exact(Arc::<Vec<u8>>::make_mut(&mut frame_data)),
                )
                .await
                .is_ok_and(|res| res.is_ok())
                {
                    let now = unix_now_f64();
                    frame_count += 1;
                    if frame_count == 1 {
                        received_any_frame = true;
                        {
                            let mut m_guard = self.camera_metrics.write().await;
                            let entry = m_guard.entry(camera.clone()).or_default();
                            entry.camera_fps = fps as f64;
                            entry.process_fps = fps as f64;
                            entry.pid = child_pid;
                            entry.last_frame_ts = now;
                        }
                        if self.comms.set_camera_availability(&camera, true) {
                            self.log_hub.push_ninjanvr(
                                "INFO",
                                &format!("ninjanvr.camera.{camera}"),
                                "Camera stream reconnected (ONLINE)",
                            );
                            self.log_hub.push_camera(
                                &camera,
                                "INFO",
                                "Camera stream reconnected (ONLINE)",
                            );
                        }
                    }

                    if now - last_live_config_check >= LIVE_VISION_CONFIG_POLL_SEC {
                        last_live_config_check = now;
                        let next_cfg = self.config.read().await.cameras.get(&camera).cloned();
                        if let Some(next_cfg) = next_cfg {
                            let delta = diff_live_vision_config(&cam_cfg, &next_cfg);
                            if delta.any() {
                                apply_live_vision_config(&mut cam_cfg, &next_cfg, &delta);
                                if delta.motion {
                                    motion_detector = SimdMotionDetector::new(
                                        width,
                                        height,
                                        cam_cfg.motion.clone(),
                                    );
                                }
                                if delta.state_rois {
                                    roi_monitor.reconfigure(&cam_cfg.state_rois);
                                }
                                if delta.regions {
                                    tracker.reconfigure_regions(&cam_cfg);
                                }
                                if delta.privacy_mask {
                                    normalized_priv_mask = normalize_privacy_mask_for_resolution(
                                        &cam_cfg.privacy_mask,
                                        width,
                                        height,
                                    );
                                }
                                info!("Applied live vision config to {camera}: {delta:?}");
                                self.log_hub.push_ninjanvr(
                                    "INFO",
                                    &format!("ninjanvr.camera.{camera}"),
                                    "Applied saved zones, masks and motion settings without restart",
                                );
                            }
                        }
                    }

                    let rt_flags = self.comms.get_camera_flags(&camera);
                    {
                        let mut ledger = self.activity_ledger.write().await;
                        ledger
                            .entry(camera.clone())
                            .or_default()
                            .set_record_enabled(rt_flags.enabled && rt_flags.record);
                    }
                    if !rt_flags.enabled {
                        if prev_motion_on {
                            prev_motion_on = false;
                            self.comms.publish_topic(&format!("{camera}/motion"), "OFF");
                        }
                        for (lbl, cnt) in &mut prev_label_counts {
                            if *cnt != 0 {
                                *cnt = 0;
                                self.comms.publish_topic(&format!("{camera}/{lbl}"), "0");
                            }
                        }
                        if prev_total_objs != 0 {
                            prev_total_objs = 0;
                            self.comms.publish_topic(&format!("{camera}/all"), "0");
                        }
                        self.comms.update_camera_activity_with_night(
                            &camera,
                            false,
                            Vec::new(),
                            0,
                            prev_night_ir,
                        );
                        if let Some(rev) = active_review.take() {
                            let dto = rev.to_dto(&camera, Some(rev.last_activity_time));
                            let db_clone = self.db.clone();
                            let dto_for_db = dto.clone();
                            match tokio::task::spawn_blocking(move || {
                                db_clone.insert_review_segment(&dto_for_db)
                            })
                            .await
                            {
                                Ok(Ok(())) => {
                                    let before_dto = rev.prev_dto.unwrap_or_else(|| dto.clone());
                                    self.comms.publish_json(
                                        "reviews",
                                        &json!({
                                            "type": "end",
                                            "before": before_dto,
                                            "after": dto
                                        }),
                                    );
                                }
                                Ok(Err(err)) => {
                                    warn!(
                                        "Failed to persist review segment on camera disable for {camera}: {err:#}"
                                    );
                                }
                                Err(join_err) => {
                                    warn!(
                                        "Blocking task failed persisting review segment on camera disable for {camera}: {join_err:#}"
                                    );
                                }
                            }
                        }
                        tokio::task::yield_now().await;
                        continue;
                    }

                    let yuv_frame = YuvFrame {
                        camera: cam_arc.clone(),
                        timestamp: now,
                        width,
                        height,
                        data: frame_data.clone(),
                    };
                    let is_night_ir = is_frame_night_ir(&yuv_frame.data, width, height);
                    tracker.set_night_mode(is_night_ir);
                    {
                        let presets_guard = self.ptz_active_presets.read().await;
                        if let Some(active_preset_opt) = presets_guard.get(&camera) {
                            tracker.set_active_preset(active_preset_opt.as_deref());
                        } else {
                            tracker.set_active_preset(cam_cfg.onvif.return_preset.as_deref());
                        }
                    }

                    let _ = self
                        .evaluate_camera_tamper(
                            &camera,
                            &yuv_frame.data,
                            width,
                            height,
                            now,
                            &mut consecutive_tamper_frames,
                            tamper_threshold_frames,
                        )
                        .await;

                    // 1. SIMD Motion Detection on Y-plane
                    let motion_boxes = if rt_flags.enabled && rt_flags.detect {
                        motion_detector.detect(&yuv_frame)
                    } else {
                        Vec::new()
                    };
                    let motion_px: u32 = motion_boxes
                        .iter()
                        .fold(0u32, |acc, b| acc.saturating_add(b.area()))
                        .min(width.saturating_mul(height));

                    if rt_flags.enabled && rt_flags.detect && !roi_monitor.is_empty() {
                        let _ = self
                            .evaluate_camera_roi_states(
                                &camera,
                                &yuv_frame.data,
                                width,
                                height,
                                now,
                                &motion_boxes,
                                &mut roi_monitor,
                                false,
                            )
                            .await;
                    }

                    // 2. Dynamic Region Cropping & AI Object Detection (only when motion or non-stationary objects exist)
                    let active_det = self
                        .detector
                        .named_detector(cam_cfg.detect.detector.as_deref())
                        .unwrap_or_else(|| self.detector.clone());
                    let (model_w, model_h, bgr_order) = active_det.model_dimensions();
                    let needed_tensor_len = (model_w as usize)
                        .saturating_mul(model_h as usize)
                        .saturating_mul(3);
                    if tensor_buf.len() != needed_tensor_len {
                        tensor_buf.resize(needed_tensor_len, 0);
                    }
                    let tracked_boxes = tracker.non_stationary_boxes(&motion_boxes);
                    let regions = if rt_flags.enabled && rt_flags.detect {
                        compute_inference_regions(
                            width,
                            height,
                            model_w.max(model_h),
                            &motion_boxes,
                            &tracked_boxes,
                        )
                    } else {
                        Vec::new()
                    };

                    let (raw_detections, returned_tensor_buf, batch_detect_calls) = if regions
                        .is_empty()
                    {
                        (Vec::new(), tensor_buf, 0u64)
                    } else {
                        let det_engine = active_det.clone();
                        let frame_for_infer = yuv_frame.clone();
                        let regions_for_infer = regions.clone();
                        let motion_for_infer = motion_boxes.clone();
                        let tracked_for_infer = tracked_boxes.clone();
                        let mut buf = std::mem::take(&mut tensor_buf);
                        match tokio::task::spawn_blocking(move || {
                            let mut dets_out = Vec::new();
                            let mut calls = 0u64;
                            for region in &regions_for_infer {
                                crop_yuv420p_to_tensor_nhwc(
                                    &frame_for_infer,
                                    region,
                                    model_w,
                                    model_h,
                                    bgr_order,
                                    &mut buf,
                                );
                                calls += 1;
                                let mut dets =
                                    det_engine.detect_region(&buf, *region, width, height, 0.40);
                                dets.retain(|d| {
                                    motion_for_infer.iter().any(|mb| mb.intersects(&d.bbox))
                                        || tracked_for_infer.iter().any(|tb| tb.intersects(&d.bbox))
                                });
                                dets_out.extend(dets);
                            }
                            (dets_out, buf, calls)
                        })
                        .await
                        {
                            Ok(res) => res,
                            Err(join_err) => {
                                warn!("Inference blocking task failed for {camera}: {join_err:#}");
                                (Vec::new(), vec![0u8; needed_tensor_len], 0)
                            }
                        }
                    };
                    tensor_buf = returned_tensor_buf;
                    detect_calls += batch_detect_calls;

                    // 3. Multi-Object Tracking (ByteTrack)
                    let (active_tracks, ended_tracks) =
                        tracker.update(now, &raw_detections, &regions, &cam_cfg);

                    // 4. Publish real-time motion, per-label MQTT counts, and WS camera_activity
                    let motion_on = !motion_boxes.is_empty();
                    let mut cur_label_counts: BTreeMap<String, usize> = BTreeMap::new();
                    for lbl in prev_label_counts.keys() {
                        cur_label_counts.insert(lbl.clone(), 0);
                    }
                    for t in &active_tracks {
                        *cur_label_counts.entry(t.label.clone()).or_default() += 1;
                    }

                    let mut activity_changed = false;
                    if motion_on != prev_motion_on {
                        prev_motion_on = motion_on;
                        activity_changed = true;
                        self.comms.publish_topic(
                            &format!("{camera}/motion"),
                            if motion_on { "ON" } else { "OFF" },
                        );
                    }

                    for (lbl, &cnt) in &cur_label_counts {
                        if prev_label_counts.get(lbl).copied().unwrap_or(0) != cnt {
                            activity_changed = true;
                            self.comms
                                .publish_topic(&format!("{camera}/{lbl}"), cnt.to_string());
                        }
                    }
                    prev_label_counts = cur_label_counts;

                    if active_tracks.len() != prev_total_objs {
                        prev_total_objs = active_tracks.len();
                        activity_changed = true;
                        self.comms
                            .publish_topic(&format!("{camera}/all"), prev_total_objs.to_string());
                    }

                    if is_night_ir != prev_night_ir {
                        prev_night_ir = is_night_ir;
                        activity_changed = true;
                    }

                    if activity_changed
                        || !active_tracks.is_empty()
                        || (motion_on && frame_count.is_multiple_of(5))
                    {
                        let max_area = motion_boxes.iter().map(|b| b.area()).max().unwrap_or(0);
                        let objs_json: Vec<Value> = active_tracks
                            .iter()
                            .map(|t| {
                                let ratio =
                                    (t.bbox.width() as f32) / (t.bbox.height().max(1) as f32);
                                json!({
                                    "id": t.id,
                                    "label": t.label,
                                    "stationary": t.stationary,
                                    "area": t.bbox.area(),
                                    "ratio": ratio,
                                    "score": t.score,
                                    "sub_label": t.inferred_sub_label
                                })
                            })
                            .collect();
                        self.comms.update_camera_activity_with_night(
                            &camera,
                            motion_on,
                            objs_json,
                            max_area,
                            is_night_ir,
                        );
                    }

                    // 5. Record activity into in-memory ring buffer for RecordingEngine
                    {
                        let mut ledger = self.activity_ledger.write().await;
                        ledger.entry(camera.clone()).or_default().record_sample(
                            now,
                            motion_px,
                            active_tracks.len() as u32,
                            regions.len() as u32,
                        );
                    }

                    // 6. Store zero-copy YuvFrame in memory on every frame for lazy high-res snapshots,
                    // and throttle disk fallback persistence (`latest_{camera}.jpg`) to every 150 frames (~30s).
                    self.latest_yuv_frames
                        .write()
                        .await
                        .insert(camera.clone(), yuv_frame.clone());
                    self.camera_metrics
                        .write()
                        .await
                        .entry(camera.clone())
                        .or_default()
                        .last_frame_ts = now;

                    // 6b. Live event-strip preview frames, sampled from this already-decoded
                    // frame: no extra decoder or ffmpeg, just a downscale + small JPEG.
                    let moving_objects = active_tracks.iter().any(|t| !t.stationary);
                    let fast_object = active_tracks.iter().any(|t| {
                        !t.stationary
                            && matches!(
                                t.label.as_str(),
                                "car" | "truck" | "bus" | "motorcycle" | "bicycle"
                            )
                    });
                    if cam_cfg.record.enabled
                        && preview_sampler.should_write(now, motion_on, moving_objects, fast_object)
                    {
                        let store = self.preview_frames.clone();
                        let frame_for_preview = yuv_frame.clone();
                        let mask_for_preview = normalized_priv_mask.clone();
                        let cam_for_preview = camera.clone();
                        let ts_ms = (now.max(0.0) * 1000.0) as u64;
                        tokio::task::spawn_blocking(move || {
                            let mut rgb = yuv420p_to_rgb_image_scaled(
                                &frame_for_preview,
                                None,
                                &[],
                                Some(crate::preview_frames::PREVIEW_FRAME_HEIGHT),
                            );
                            apply_privacy_masks_from_value(&mut rgb, &mask_for_preview);
                            match encode_image_jpeg_with_quality(
                                &rgb,
                                crate::preview_frames::PREVIEW_FRAME_JPEG_QUALITY,
                            ) {
                                Ok(jpeg) => {
                                    if let Err(err) =
                                        store.write_frame(&cam_for_preview, ts_ms, &jpeg)
                                    {
                                        warn!(
                                            "Failed to write preview frame for {cam_for_preview}: {err}"
                                        );
                                    }
                                }
                                Err(err) => {
                                    warn!(
                                        "Failed to encode preview frame for {cam_for_preview}: {err:#}"
                                    );
                                }
                            }
                        });
                    }

                    if frame_count == 1 || frame_count.is_multiple_of(150) {
                        let annotated: Vec<(BoundingBox, [u8; 3])> =
                            if cam_cfg.snapshots.bounding_box {
                                active_tracks
                                    .iter()
                                    .map(|t| (t.bbox, [31, 119, 180]))
                                    .collect()
                            } else {
                                Vec::new()
                            };
                        let frame_for_latest = yuv_frame.clone();
                        let mask_for_latest = normalized_priv_mask.clone();
                        let media_root_for_latest = self.media_dir.clone();
                        let clips_dir_for_latest = self.media_dir.join("clips");
                        let latest_name = format!("latest_{safe_cam_file}.jpg");
                        if let Ok(Some(jpeg_bytes)) = tokio::task::spawn_blocking(move || {
                            let mut rgb = yuv420p_to_rgb_image_scaled(
                                &frame_for_latest,
                                None,
                                &annotated,
                                Some(360),
                            );
                            apply_privacy_masks_from_value(&mut rgb, &mask_for_latest);
                            let Ok(jpeg_bytes) = encode_image_jpeg(&rgb) else {
                                return None;
                            };
                            if let Err(err) = write_contained_media_file(
                                &media_root_for_latest,
                                &clips_dir_for_latest,
                                &latest_name,
                                &jpeg_bytes,
                            ) {
                                warn!("Failed to write latest frame {latest_name}: {err:#}");
                            }
                            Some(jpeg_bytes)
                        })
                        .await
                        {
                            self.latest_frames
                                .write()
                                .await
                                .insert(camera.clone(), jpeg_bytes);
                        }
                    }

                    // 7. Persist Events, Timeline entries, and ReviewSegments
                    let ctx = FrameLifecycleCtx {
                        camera: &camera,
                        cam_cfg: &cam_cfg,
                        frame: &yuv_frame,
                        now,
                    };
                    self.process_lifecycle_events(
                        &ctx,
                        &active_tracks,
                        &ended_tracks,
                        &mut active_review,
                        &mut track_snapshots,
                    )
                    .await;

                    // 8. Update rolling FPS metrics every 5 seconds
                    let elapsed = (now - window_start).max(0.1);
                    if elapsed >= 5.0 {
                        let c_fps = ((frame_count as f64 / elapsed) * 10.0).round() / 10.0;
                        let d_fps = ((detect_calls as f64 / elapsed) * 10.0).round() / 10.0;
                        self.camera_metrics.write().await.insert(
                            camera.clone(),
                            CameraRuntimeMetrics {
                                camera_fps: c_fps,
                                process_fps: c_fps,
                                detection_fps: d_fps,
                                pid: child_pid,
                                last_frame_ts: now,
                                night_mode: is_night_ir,
                            },
                        );
                        frame_count = 0;
                        detect_calls = 0;
                        window_start = now;
                    }
                    tokio::task::yield_now().await;
                }
            }

            {
                let mut m_guard = self.camera_metrics.write().await;
                if let Some(m) = m_guard.get_mut(&camera) {
                    m.camera_fps = 0.0;
                    m.process_fps = 0.0;
                    m.detection_fps = 0.0;
                    m.pid = 0;
                }
            }
            self.comms.publish_topic(&format!("{camera}/motion"), "OFF");
            for lbl in &cam_cfg.objects.track {
                self.comms.publish_topic(&format!("{camera}/{lbl}"), "0");
            }
            self.comms.publish_topic(&format!("{camera}/all"), "0");
            self.comms
                .update_camera_activity_with_night(&camera, false, Vec::new(), 0, false);
            if let Some(rev) = active_review.take() {
                let dto = rev.to_dto(&camera, Some(rev.last_activity_time));
                let db_clone = self.db.clone();
                let dto_for_db = dto.clone();
                match tokio::task::spawn_blocking(move || {
                    db_clone.insert_review_segment(&dto_for_db)
                })
                .await
                {
                    Ok(Ok(())) => {
                        let before_dto = rev.prev_dto.unwrap_or_else(|| dto.clone());
                        self.comms.publish_json(
                            "reviews",
                            &json!({
                                "type": "end",
                                "before": before_dto,
                                "after": dto
                            }),
                        );
                    }
                    Ok(Err(err)) => {
                        warn!(
                            "Failed to persist review segment on stream disconnect for {camera}: {err:#}"
                        );
                    }
                    Err(join_err) => {
                        warn!(
                            "Blocking task failed persisting review segment on stream disconnect for {camera}: {join_err:#}"
                        );
                    }
                }
            }
            track_snapshots.clear();
            if self.comms.set_camera_availability(&camera, false) {
                self.log_hub.push_ninjanvr(
                    "WARNING",
                    &format!("ninjanvr.camera.{camera}"),
                    "Camera stream disconnected (OFFLINE)",
                );
                self.log_hub.push_camera(
                    &camera,
                    "WARNING",
                    "Camera stream disconnected (OFFLINE)",
                );
            }
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            let retry_sec = if received_any_frame {
                consecutive_stall_cycles = 0;
                5
            } else {
                consecutive_stall_cycles = consecutive_stall_cycles.saturating_add(1);
                let now_ts = unix_now_f64();
                if consecutive_stall_cycles >= 3 && now_ts - last_watchdog_reboot_ts >= 1800.0 {
                    last_watchdog_reboot_ts = now_ts;
                    let cfg_snap = self.config.read().await.clone();
                    if try_watchdog_camera_reboot(&cfg_snap, &camera, &self.camera_driver_runtime)
                        .await
                    {
                        self.log_hub.push_ninjanvr(
                            "WARNING",
                            &format!("ninjanvr.camera.{camera}"),
                            "RTSP stream stalled for 3 consecutive cycles; triggered automatic ONVIF/CGI SystemReboot recovery",
                        );
                        self.log_hub.push_camera(
                            &camera,
                            "WARNING",
                            "RTSP stream stalled for 3 consecutive cycles; triggered automatic ONVIF/CGI SystemReboot recovery",
                        );
                        warn!(
                            "Triggered automatic ONVIF/CGI SystemReboot recovery for stalled camera {camera}; waiting 35s for boot"
                        );
                        35
                    } else {
                        10
                    }
                } else {
                    10
                }
            };
            tokio::time::sleep(Duration::from_secs(retry_sec)).await;
        }
    }

    /// Evaluates a YUV420p frame for camera tamper / occlusion hysteresis (`tamper_threshold_frames`
    /// consecutive blind/flat frames) and updates `CommsHub`, `LogBufferHub`, and SQLite `reviewsegment`.
    /// Returns `true` when the camera is currently in an active tamper state.
    // Frame-loop evaluation passing borrowed frame buffer and mutable hysteresis counter directly
    #[expect(clippy::too_many_arguments)]
    pub async fn evaluate_camera_tamper(
        &self,
        camera: &str,
        yuv_data: &[u8],
        width: u32,
        height: u32,
        now: f64,
        consecutive_tamper_frames: &mut u32,
        tamper_threshold_frames: u32,
    ) -> bool {
        let tamper_report = check_frame_tamper(yuv_data, width, height);
        let threshold = tamper_threshold_frames.max(1);
        if tamper_report.is_tampered_or_blind {
            *consecutive_tamper_frames = consecutive_tamper_frames.saturating_add(1);
            if *consecutive_tamper_frames >= threshold {
                if *consecutive_tamper_frames == threshold
                    && self.comms.set_camera_tamper(camera, true)
                {
                    self.log_hub.push_ninjanvr(
                        "WARNING",
                        &format!("ninjanvr.vision.{camera}"),
                        &format!(
                            "Camera tamper / blind feed detected (uniform_ratio={:.2}, y_variance={:.2})",
                            tamper_report.uniform_ratio, tamper_report.y_variance
                        ),
                    );
                    let tamper_id = format!("{now:.6}-tamper-{}", random_suffix());
                    let dto = ReviewItemDto {
                        id: tamper_id,
                        camera: camera.to_string(),
                        start_time: now,
                        end_time: None,
                        severity: "alert".to_string(),
                        thumb_path: format!("/api/{camera}/latest.jpg"),
                        data: json!({
                            "detections": [],
                            "objects": ["camera_tamper"],
                            "zones": [],
                            "audio": [],
                            "thumb_time": now,
                            "metadata": {
                                "uniform_ratio": tamper_report.uniform_ratio,
                                "y_variance": tamper_report.y_variance
                            }
                        }),
                        has_been_reviewed: false,
                    };
                    let db_clone = self.db.clone();
                    let insert_res =
                        tokio::task::spawn_blocking(move || db_clone.insert_review_segment(&dto))
                            .await
                            .unwrap_or_else(|e| {
                                Err(anyhow::anyhow!("spawn_blocking join error: {e}"))
                            });
                    if let Err(err) = insert_res {
                        warn!(
                            "Failed to persist camera tamper review segment for {camera}: {err:#}"
                        );
                    }
                }
                return true;
            }
            false
        } else {
            *consecutive_tamper_frames = 0;
            let _ = self.comms.set_camera_tamper(camera, false);
            false
        }
    }

    /// Evaluates all configured `state_rois` on a camera frame (respecting `interval_sec`,
    /// motion-settle triggers, and `hysteresis`), publishes transitions over MQTT and `/ws`,
    /// and emits a `ReviewSegment` alert when an `anomaly_state` persists `>= anomaly_timeout_sec`.
    // Frame-loop ROI state evaluation passing borrowed frame buffer, motion boxes, and monitor state directly
    #[expect(clippy::too_many_arguments)]
    pub async fn evaluate_camera_roi_states(
        &self,
        camera: &str,
        yuv_data: &[u8],
        width: u32,
        height: u32,
        now: f64,
        motion_boxes: &[BoundingBox],
        roi_monitor: &mut CameraRoiStateMonitor,
        force_eval: bool,
    ) -> Vec<RoiStateTransition> {
        if roi_monitor.is_empty() {
            return Vec::new();
        }
        let transitions =
            roi_monitor.evaluate_frame(yuv_data, width, height, now, motion_boxes, force_eval);
        let existing_states = self.comms.get_camera_roi_states(camera);
        for tr in &transitions {
            let prev_published = existing_states.get(&tr.roi_name).map(String::as_str);
            if tr.state_changed || prev_published != Some(tr.current_state.as_str()) {
                self.comms
                    .publish_roi_state(camera, &tr.roi_name, &tr.current_state);
            }
            if tr.anomaly_triggered {
                self.emit_roi_anomaly_review_alert(camera, tr, now).await;
            }
        }
        transitions
    }

    /// Records a classified ROI state observation into `roi_monitor` for `roi_name`,
    /// publishing state transitions via `CommsHub` and persisting a `ReviewSegment` alert
    /// when `anomaly_state` persists `>= anomaly_timeout_sec`.
    pub async fn record_roi_state_observation(
        &self,
        camera: &str,
        roi_monitor: &mut CameraRoiStateMonitor,
        roi_name: &str,
        candidate_state: &str,
        confidence: f32,
        now: f64,
    ) -> Option<RoiStateTransition> {
        let tracker = roi_monitor.tracker_mut(roi_name)?;
        let tr = tracker.observe_state(candidate_state, confidence, now);
        let existing_states = self.comms.get_camera_roi_states(camera);
        let prev_published = existing_states.get(&tr.roi_name).map(String::as_str);
        if tr.state_changed || prev_published != Some(tr.current_state.as_str()) {
            self.comms
                .publish_roi_state(camera, &tr.roi_name, &tr.current_state);
        }
        if tr.anomaly_triggered {
            self.emit_roi_anomaly_review_alert(camera, &tr, now).await;
        }
        Some(tr)
    }

    async fn emit_roi_anomaly_review_alert(&self, camera: &str, tr: &RoiStateTransition, now: f64) {
        self.log_hub.push_ninjanvr(
            "WARNING",
            &format!("ninjanvr.vision.{camera}"),
            &format!(
                "ROI '{}' remained in anomalous state '{}' for {:.0}s",
                tr.roi_name, tr.current_state, tr.anomaly_elapsed_sec
            ),
        );
        let safe_roi = sanitize_filename_component(&tr.roi_name);
        let alert_id = format!("{now:.6}-roi-{safe_roi}-{}", random_suffix());
        let dto = ReviewItemDto {
            id: alert_id,
            camera: camera.to_string(),
            start_time: now,
            end_time: None,
            severity: "alert".to_string(),
            thumb_path: format!("/api/{camera}/latest.jpg"),
            data: json!({
                "detections": [],
                "objects": [format!("roi:{}:{}", tr.roi_name, tr.current_state)],
                "zones": [tr.roi_name],
                "audio": [],
                "thumb_time": now,
                "metadata": {
                    "roi_name": tr.roi_name,
                    "state": tr.current_state,
                    "anomaly_elapsed_sec": tr.anomaly_elapsed_sec
                }
            }),
            has_been_reviewed: false,
        };
        let db_clone = self.db.clone();
        let dto_for_db = dto.clone();
        let insert_res =
            tokio::task::spawn_blocking(move || db_clone.insert_review_segment(&dto_for_db))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("spawn_blocking join error: {e}")));
        match insert_res {
            Ok(()) => {
                self.comms.publish_json(
                    "reviews",
                    &json!({
                        "type": "new",
                        "before": dto,
                        "after": dto
                    }),
                );
            }
            Err(err) => {
                warn!(
                    "Failed to persist ROI anomaly review segment for {camera} (roi={}): {err:#}",
                    tr.roi_name
                );
            }
        }
    }

    async fn prune_false_positive_track_from_review(
        &self,
        active_review: &mut Option<ActiveReviewState>,
        track_id: &str,
        camera: &str,
        now: f64,
    ) {
        let remaining_det_ids: Vec<String> = match active_review.as_ref() {
            Some(rev) if rev.detections.iter().any(|d| d == track_id) => rev
                .detections
                .iter()
                .filter(|d| *d != track_id)
                .cloned()
                .collect(),
            _ => return,
        };
        let has_remaining_alert = if remaining_det_ids.is_empty() {
            false
        } else {
            let db_clone = self.db.clone();
            tokio::task::spawn_blocking(move || {
                remaining_det_ids.iter().any(|det_id| {
                    db_clone.get_event(det_id).ok().flatten().is_some_and(|ev| {
                        ev["false_positive"].as_bool() != Some(true)
                            && ev["data"]["max_severity"].as_str() == Some("alert")
                    })
                })
            })
            .await
            .unwrap_or(false)
        };
        if let ReviewFalsePositiveTransition::DeleteEmptyReview {
            review_id,
            persisted_in_db: true,
            before_dto,
            closed_dto,
        } = prune_false_positive_from_active_review(active_review, track_id, camera, now, |_| {
            has_remaining_alert
        }) {
            let db_for_del = self.db.clone();
            let rev_id_for_del = review_id.clone();
            match tokio::task::spawn_blocking(move || {
                db_for_del.delete_reviews(std::slice::from_ref(&rev_id_for_del))
            })
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    warn!("Failed to delete false-positive review {review_id}: {err:#}");
                }
                Err(join_err) => {
                    warn!(
                        "Blocking task failed deleting false-positive review {review_id}: {join_err:#}"
                    );
                }
            }
            self.comms.publish_json(
                "reviews",
                &json!({
                    "type": "end",
                    "before": before_dto,
                    "after": closed_dto
                }),
            );
        }
    }

    pub async fn spawn_camera_siren_for_trigger(
        &self,
        camera: &str,
        cam_cfg: &CameraConfig,
        trigger_source: &str,
        object_label: &str,
        now: f64,
    ) {
        if !self
            .camera_driver_runtime
            .try_claim_siren_trigger(camera, now, 15.0)
        {
            return;
        }
        let effective_cam_cfg = {
            let cfg_guard = self.config.read().await;
            crate::camera_protocols::enrich_camera_config_from_upstream(&cfg_guard, camera, cam_cfg)
        };
        let target = self
            .camera_driver_runtime
            .resolve_probe_target(camera, &effective_cam_cfg);
        if target.host.is_none() {
            return;
        }

        self.log_hub.push_ninjanvr(
            "INFO",
            &format!("ninjanvr.siren.{camera}"),
            &format!(
                "Triggered audible camera alarm on {camera} (trigger={trigger_source}, label={object_label})"
            ),
        );
        self.comms.publish_json(
            "siren",
            &json!({
                "camera": camera,
                "trigger": trigger_source,
                "label": object_label,
                "timestamp": now
            }),
        );

        let Some(client) = self.snapshot_http_client.clone() else {
            return;
        };
        let rt = self.camera_driver_runtime.clone();
        let cam_owned = camera.to_string();
        let trig_owned = trigger_source.to_string();
        let log_hub = self.log_hub.clone();
        tokio::spawn(async move {
            if let Err(err_msg) = rt
                .trigger_camera_siren(&client, &cam_owned, &effective_cam_cfg, 3.5)
                .await
            {
                log_hub.push_ninjanvr(
                    "WARN",
                    &format!("ninjanvr.siren.{cam_owned}"),
                    &format!("Camera siren failed on {cam_owned} ({trig_owned}): {err_msg}"),
                );
            }
        });
    }

    pub async fn process_lifecycle_events(
        &self,
        ctx: &FrameLifecycleCtx<'_>,
        active_tracks: &[TrackedObject],
        ended_tracks: &[TrackedObject],
        active_review: &mut Option<ActiveReviewState>,
        track_snapshots: &mut HashMap<String, TrackPersistSnapshot>,
    ) {
        let camera = ctx.camera;
        let cam_cfg = ctx.cam_cfg;
        let Some(safe_frame) = normalize_yuv_frame_for_processing(ctx.frame) else {
            return;
        };
        let frame = &safe_frame;
        let now = ctx.now;
        let review_dir = self.media_dir.join("clips").join("review");
        let clips_dir = self.media_dir.join("clips");
        let safe_cam_file = sanitize_filename_component(camera);
        let normalized_priv_mask =
            normalize_privacy_mask_for_resolution(&cam_cfg.privacy_mask, frame.width, frame.height);

        bound_track_snapshots_capacity(track_snapshots, active_tracks, ended_tracks);

        let active_profile = self.comms.get_active_profile();
        let cam_rt_flags = self.comms.get_camera_flags(camera);
        let (
            camera_suppressed_in_profile,
            camera_record_disabled_in_profile,
            genai_cfg,
            face_cfg,
            lpr_cfg,
        ) = {
            let cfg_guard = self.config.read().await;
            let sup_alerts =
                is_camera_alert_suppressed_in_profile(&cfg_guard, &active_profile, camera);
            let prof_cfg = lookup_profile_override(&cfg_guard, &active_profile);
            let sup_rec = prof_cfg.is_some_and(|p| {
                p.disabled_record_cameras
                    .iter()
                    .any(|c| c == camera || c == "*")
            });
            (
                sup_alerts,
                sup_rec,
                cfg_guard.genai.clone(),
                cfg_guard.face_recognition.clone(),
                cfg_guard.lpr.clone(),
            )
        };
        let faces_dir = self.media_dir.join("faces");
        let snapshots_active = cam_cfg.snapshots.enabled && cam_rt_flags.snapshots;
        let record_active =
            cam_cfg.record.enabled && cam_rt_flags.record && !camera_record_disabled_in_profile;
        let mut cached_redacted_full_rgb: Option<RgbImage> = None;

        for trk in active_tracks {
            let prev = track_snapshots.get(&trk.id).cloned();
            let lpr_consensus = if lpr_cfg.enabled {
                if let Some(ref cand) = trk.inferred_sub_label
                    && ninjanvr_infer::validate_license_plate(cand, lpr_cfg.format_regex.as_deref())
                {
                    let read_score = trk
                        .attributes
                        .iter()
                        .find(|a| a.label.eq_ignore_ascii_case("license_plate"))
                        .map_or(trk.score, |a| a.score);
                    self.detector.record_lpr_track_read(
                        &trk.id,
                        ninjanvr_infer::OcrPlateRead::new(cand.clone(), read_score),
                        &lpr_cfg,
                    )
                } else {
                    self.detector.lpr_consensus_for_track(&trk.id, &lpr_cfg)
                }
            } else {
                None
            };

            let face_match = if let Some(face_crop_box) = resolve_face_recognition_candidate_bbox(
                trk,
                prev.as_ref(),
                lpr_consensus.as_ref(),
                &face_cfg,
                frame.width,
                frame.height,
            ) {
                let full_redacted = cached_redacted_full_rgb.get_or_insert_with(|| {
                    let mut img = yuv420p_to_rgb_image(frame, None, &[]);
                    apply_privacy_masks_from_value(&mut img, &normalized_priv_mask);
                    img
                });
                let face_rgb = crop_and_scale_rgb_square(full_redacted, face_crop_box, 112);
                let det_clone = self.detector.clone();
                let faces_dir_clone = faces_dir.clone();
                let sim_thresh = face_cfg.similarity_threshold;
                let unk_alert = face_cfg.unknown_alert;
                tokio::task::spawn_blocking(move || {
                    det_clone.match_face_crop(
                        &faces_dir_clone,
                        face_rgb.as_raw(),
                        sim_thresh,
                        unk_alert,
                    )
                })
                .await
                .ok()
                .flatten()
                .map(|m| (m.sub_label, m.score))
            } else {
                None
            };

            let prev_suppressed = prev.as_ref().is_some_and(|p| p.suppressed_fp);
            let enriched = enrich_tracked_object(TrackEnrichmentInput {
                trk,
                prev: prev.as_ref(),
                lpr_consensus: lpr_consensus.as_ref(),
                face_match,
                frame,
                cam_cfg,
                active_profile: &active_profile,
                camera_suppressed_in_profile,
                alerts_enabled: cam_rt_flags.alerts,
                suppressed_fp: prev_suppressed,
                is_ended: false,
            });

            let mut suppressed_fp = prev_suppressed;
            let mut matched_fp_sig_id: Option<i64> =
                prev.as_ref().and_then(|p| p.matched_fp_sig_id);
            if !suppressed_fp {
                let db_for_fp = self.db.clone();
                let cam_for_fp = camera.to_string();
                let lbl_for_fp = trk.label.clone();
                let trk_id_for_fp = trk.id.clone();
                let norm_box = enriched.norm_box;
                let crop_dhash64 = enriched.crop_dhash64;
                if enriched.is_new {
                    if let Ok(Ok(Some(sig_val))) = tokio::task::spawn_blocking(move || {
                        db_for_fp.match_false_positive_signature(
                            &cam_for_fp,
                            &lbl_for_fp,
                            norm_box,
                            Some(crop_dhash64),
                        )
                    })
                    .await
                    {
                        suppressed_fp = true;
                        matched_fp_sig_id = sig_val["id"].as_i64();
                        self.log_hub.push_ninjanvr(
                            "INFO",
                            &format!("ninjanvr.vision.{camera}"),
                            &format!(
                                "Suppressed recurring false-positive track {} ({}) matching signature #{:?}",
                                trk.id, trk.label, matched_fp_sig_id
                            ),
                        );
                    }
                } else if let Ok(Ok(Some(ev_row))) =
                    tokio::task::spawn_blocking(move || db_for_fp.get_event(&trk_id_for_fp)).await
                    && ev_row["false_positive"].as_bool() == Some(true)
                {
                    suppressed_fp = true;
                    if matched_fp_sig_id.is_none() {
                        matched_fp_sig_id = ev_row["data"]["suppressed_by_fp_signature"].as_i64();
                    }
                }
            }
            let fp_just_activated = suppressed_fp && !prev_suppressed && !enriched.is_new;
            if suppressed_fp {
                self.prune_false_positive_track_from_review(active_review, &trk.id, camera, now)
                    .await;
            }
            let mut ai_verify_triggered = prev.as_ref().is_some_and(|p| p.ai_verify_triggered);

            if enriched.is_new
                || enriched.is_better_score
                || enriched.is_better_attr
                || enriched.state_changed
                || enriched.attrs_changed
                || fp_just_activated
            {
                let should_trigger_ai = !suppressed_fp
                    && !ai_verify_triggered
                    && genai_cfg.enabled
                    && genai_cfg.verify_ambiguous_alerts
                    && trk.score >= genai_cfg.ambiguous_score_min
                    && trk.score <= genai_cfg.ambiguous_score_max;
                let genai_permit_opt = if should_trigger_ai {
                    self.genai_verify_semaphore.clone().try_acquire_owned().ok()
                } else {
                    None
                };
                if genai_permit_opt.is_some() {
                    ai_verify_triggered = true;
                }
                track_snapshots.insert(
                    trk.id.clone(),
                    TrackPersistSnapshot {
                        best_score: enriched.best_score,
                        best_attribute_score: enriched.best_attr_score,
                        sub_label: enriched.effective_sub_label.clone(),
                        sub_label_score: enriched.effective_sub_label_score,
                        detected_attributes: enriched.merged_attributes.clone(),
                        zones: trk.entered_zones.clone(),
                        stationary: trk.stationary,
                        dhash64: Some(enriched.crop_dhash64),
                        suppressed_fp,
                        matched_fp_sig_id,
                        ai_verify_triggered,
                    },
                );

                let mut thumb_b64 = None;
                let replace_snap = should_replace_snapshot(
                    cam_cfg.snapshots.selection,
                    enriched.is_new,
                    enriched.prev_best_score,
                    trk.score,
                    enriched.prev_best_attr_quality,
                    enriched.new_attr_quality,
                    &trk.zones,
                    &trk.entered_zones,
                );
                if replace_snap {
                    let clean_img = cached_redacted_full_rgb
                        .get_or_insert_with(|| {
                            let mut img = yuv420p_to_rgb_image(frame, None, &[]);
                            apply_privacy_masks_from_value(&mut img, &normalized_priv_mask);
                            img
                        })
                        .clone();
                    let safe_trk_id = sanitize_filename_component(&trk.id);
                    let jpg_name = format!("{safe_trk_id}.jpg");
                    let webp_name = format!("{safe_cam_file}-{safe_trk_id}.webp");
                    let clean_jpg_name = format!("{safe_trk_id}-clean.jpg");
                    let clean_webp_name = format!("{safe_cam_file}-{safe_trk_id}-clean.webp");

                    let frame_for_snap = frame.clone();
                    let region_for_snap = trk.region;
                    let bbox_for_snap = trk.bbox;
                    let draw_bbox_snap = cam_cfg.snapshots.bounding_box;
                    let mask_for_snap = normalized_priv_mask.clone();
                    let media_root_for_snap = self.media_dir.clone();
                    let clips_dir_for_snap = clips_dir.clone();
                    let do_disk_snaps = snapshots_active;
                    let jpg_name_for_snap = jpg_name.clone();
                    let webp_name_for_snap = webp_name.clone();
                    let clean_jpg_for_snap = clean_jpg_name.clone();
                    let clean_webp_for_snap = clean_webp_name.clone();

                    if let Ok(encoded_b64) = tokio::task::spawn_blocking(move || {
                        let crop_img =
                            crop_and_scale_rgb_image(&clean_img, region_for_snap, Some(175));
                        let b64 = encode_image_webp(&crop_img)
                            .ok()
                            .map(|b| base64::engine::general_purpose::STANDARD.encode(b));

                        if do_disk_snaps {
                            if let Ok(clean_webp) = encode_image_webp(&clean_img)
                                && let Err(err) = write_contained_media_file(
                                    &media_root_for_snap,
                                    &clips_dir_for_snap,
                                    &clean_webp_for_snap,
                                    &clean_webp,
                                )
                            {
                                warn!("Failed to write snapshot {clean_webp_for_snap}: {err:#}");
                            }
                            if let Ok(clean_jpg) = encode_image_jpeg(&clean_img)
                                && let Err(err) = write_contained_media_file(
                                    &media_root_for_snap,
                                    &clips_dir_for_snap,
                                    &clean_jpg_for_snap,
                                    &clean_jpg,
                                )
                            {
                                warn!("Failed to write snapshot {clean_jpg_for_snap}: {err:#}");
                            }

                            let mut full_img = clean_img;
                            if draw_bbox_snap {
                                annotate_rgb_image_with_boxes(
                                    &mut full_img,
                                    &[(bbox_for_snap, [31, 119, 180])],
                                );
                                apply_privacy_masks_from_value(&mut full_img, &mask_for_snap);
                            }
                            if let Ok(jpg) = encode_image_jpeg(&full_img)
                                && let Err(err) = write_contained_media_file(
                                    &media_root_for_snap,
                                    &clips_dir_for_snap,
                                    &jpg_name_for_snap,
                                    &jpg,
                                )
                            {
                                warn!("Failed to write snapshot {jpg_name_for_snap}: {err:#}");
                            }
                            let boxes_for_thumb: &[(BoundingBox, [u8; 3])] = if draw_bbox_snap {
                                &[(bbox_for_snap, [31, 119, 180])]
                            } else {
                                &[]
                            };
                            let mut thumb_img = yuv420p_to_rgb_image_scaled(
                                &frame_for_snap,
                                None,
                                boxes_for_thumb,
                                Some(360),
                            );
                            apply_privacy_masks_from_value(&mut thumb_img, &mask_for_snap);
                            if let Ok(webp) = encode_image_webp(&thumb_img)
                                && let Err(err) = write_contained_media_file(
                                    &media_root_for_snap,
                                    &clips_dir_for_snap,
                                    &webp_name_for_snap,
                                    &webp,
                                )
                            {
                                warn!("Failed to write snapshot {webp_name_for_snap}: {err:#}");
                            }
                        }
                        b64
                    })
                    .await
                    {
                        thumb_b64 = encoded_b64;
                    }

                    if snapshots_active
                        && cam_cfg.snapshots.high_res
                        && cam_cfg.snapshots.from_main_stream
                        && let Some(client) = self.snapshot_http_client.clone()
                        && let Ok(permit) =
                            self.high_res_snapshot_semaphore.clone().try_acquire_owned()
                    {
                        let safe_src = sanitize_filename_component(camera);
                        let det_bbox = trk.bbox;
                        let det_w = frame.width;
                        let det_h = frame.height;
                        let draw_bbox = cam_cfg.snapshots.bounding_box;
                        let priv_mask = normalized_priv_mask.clone();
                        let media_root_clone = self.media_dir.clone();
                        let clips_dir_clone = clips_dir.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            let url =
                                format!("http://127.0.0.1:1984/api/frame.jpeg?src={safe_src}");
                            if let Ok(mut resp) = client.get(&url).send().await
                                && resp.status().is_success()
                                && resp
                                    .content_length()
                                    .is_none_or(|len| len <= MAX_HIGH_RES_SNAPSHOT_BYTES as u64)
                            {
                                let mut bytes = Vec::new();
                                let mut within_cap = true;
                                while let Ok(Some(chunk)) = resp.chunk().await {
                                    if bytes.len().saturating_add(chunk.len())
                                        > MAX_HIGH_RES_SNAPSHOT_BYTES
                                    {
                                        within_cap = false;
                                        break;
                                    }
                                    bytes.extend_from_slice(&chunk);
                                }
                                if within_cap && !bytes.is_empty() {
                                    let _ = tokio::task::spawn_blocking(move || {
                                        if let Some(mut hr_rgb) =
                                            decode_bounded_high_res_snapshot(&bytes, det_w, det_h)
                                        {
                                            apply_privacy_masks_from_value(&mut hr_rgb, &priv_mask);
                                            if let Ok(hr_clean_webp) = encode_image_webp(&hr_rgb) {
                                                let _ = write_contained_media_file(
                                                    &media_root_clone,
                                                    &clips_dir_clone,
                                                    &clean_webp_name,
                                                    &hr_clean_webp,
                                                );
                                            }
                                            if let Ok(hr_clean_jpg) = encode_image_jpeg(&hr_rgb) {
                                                let _ = write_contained_media_file(
                                                    &media_root_clone,
                                                    &clips_dir_clone,
                                                    &clean_jpg_name,
                                                    &hr_clean_jpg,
                                                );
                                            }
                                            if draw_bbox {
                                                let projected = det_bbox.project_to_resolution(
                                                    det_w,
                                                    det_h,
                                                    hr_rgb.width(),
                                                    hr_rgb.height(),
                                                );
                                                annotate_rgb_image_with_boxes(
                                                    &mut hr_rgb,
                                                    &[(projected, [31, 119, 180])],
                                                );
                                                apply_privacy_masks_from_value(
                                                    &mut hr_rgb,
                                                    &priv_mask,
                                                );
                                            }
                                            if let Ok(hr_jpg) = encode_image_jpeg(&hr_rgb) {
                                                let _ = write_contained_media_file(
                                                    &media_root_clone,
                                                    &clips_dir_clone,
                                                    &jpg_name,
                                                    &hr_jpg,
                                                );
                                            }
                                            if let Ok(hr_webp) = encode_image_webp(&hr_rgb) {
                                                let _ = write_contained_media_file(
                                                    &media_root_clone,
                                                    &clips_dir_clone,
                                                    &webp_name,
                                                    &hr_webp,
                                                );
                                            }
                                        }
                                    })
                                    .await;
                                }
                            }
                        });
                    }
                }

                let severity = enriched.resolve_severity(
                    trk,
                    cam_cfg,
                    &active_profile,
                    camera_suppressed_in_profile,
                    cam_rt_flags.alerts,
                    suppressed_fp,
                );
                let ev_dto = build_track_event_dto(BuildTrackEventDtoParams {
                    camera,
                    trk,
                    enriched: &enriched,
                    record_active,
                    snapshots_active,
                    suppressed_fp,
                    matched_fp_sig_id,
                    severity,
                    thumbnail: thumb_b64,
                    end_time: None,
                    night_mode: Some(is_frame_night_ir(&frame.data, frame.width, frame.height)),
                });

                let db_for_ev = self.db.clone();
                let ev_dto_for_db = ev_dto.clone();
                let sub_label_for_db = enriched.effective_sub_label.clone();
                let sub_score_for_db = enriched.effective_sub_label_score.map(|v| v as f64);
                let trk_id_for_db = trk.id.clone();
                let cam_for_db = camera.to_string();
                let trk_label_for_db = trk.label.clone();
                let trk_score_for_db = trk.score;
                let entered_zones_for_db = trk.entered_zones.clone();
                let newly_entered_for_db = enriched.newly_entered_zones.clone();
                let norm_box = enriched.norm_box;
                let norm_reg = enriched.norm_reg;
                let best_score = enriched.best_score;
                let is_new = enriched.is_new;

                let event_persisted = match tokio::task::spawn_blocking(move || {
                    if let Err(err) = db_for_ev.upsert_event(&ev_dto_for_db) {
                        warn!(
                            "Failed to upsert active event {} for {}: {err:#}",
                            trk_id_for_db, cam_for_db
                        );
                        return false;
                    }
                    if let Some(ref sl) = sub_label_for_db
                        && let Err(err) = db_for_ev.set_event_sub_label_with_score(
                            &trk_id_for_db,
                            Some(sl),
                            sub_score_for_db,
                        )
                    {
                        warn!(
                            "Failed to persist sub_label for event {} on {}: {err:#}",
                            trk_id_for_db, cam_for_db
                        );
                    }
                    if is_new
                        && let Err(err) = db_for_ev.insert_timeline_entry(
                            now,
                            &cam_for_db,
                            "tracked_object",
                            Some(&trk_id_for_db),
                            "visible",
                            &json!({
                                "box": norm_box,
                                "label": trk_label_for_db,
                                "region": norm_reg,
                                "score": trk_score_for_db,
                                "top_score": best_score
                            }),
                        )
                    {
                        warn!(
                            "Failed to insert visible timeline entry for {} on {}: {err:#}",
                            trk_id_for_db, cam_for_db
                        );
                    }
                    if !newly_entered_for_db.is_empty()
                        && let Err(err) = db_for_ev.insert_timeline_entry(
                            now,
                            &cam_for_db,
                            "tracked_object",
                            Some(&trk_id_for_db),
                            "entered_zone",
                            &json!({
                                "box": norm_box,
                                "label": trk_label_for_db,
                                "region": norm_reg,
                                "zones": entered_zones_for_db,
                                "entered_zones": newly_entered_for_db,
                                "score": trk_score_for_db,
                                "top_score": best_score
                            }),
                        )
                    {
                        warn!(
                            "Failed to insert entered_zone timeline entry for {} on {}: {err:#}",
                            trk_id_for_db, cam_for_db
                        );
                    }
                    true
                })
                .await
                {
                    Ok(ok) => ok,
                    Err(join_err) => {
                        warn!(
                            "Blocking task failed persisting active event {} on {camera}: {join_err:#}",
                            trk.id
                        );
                        false
                    }
                };

                if event_persisted {
                    if let Some(genai_permit) = genai_permit_opt {
                        let cfg_clone = self.config.clone();
                        let db_clone = self.db.clone();
                        let media_clone = self.media_dir.clone();
                        let ev_id_owned = trk.id.clone();
                        tokio::spawn(async move {
                            let _permit = genai_permit;
                            let _ = tokio::time::timeout(
                                GENAI_VERIFY_TIMEOUT,
                                crate::verify_event_with_genai_components(
                                    cfg_clone,
                                    db_clone,
                                    media_clone,
                                    &ev_id_owned,
                                ),
                            )
                            .await;
                        });
                    }

                    self.comms.publish_json(
                        "events",
                        &json!({
                            "type": if is_new { "new" } else { "update" },
                            "before": ev_dto,
                            "after": ev_dto
                        }),
                    );
                    if is_new {
                        debug!("New tracked object on {camera}: {} ({})", trk.label, trk.id);
                        self.log_hub.push_ninjanvr(
                            "INFO",
                            &format!("ninjanvr.detect.{camera}"),
                            &format!(
                                "Tracked object {}: {} (score={:.2}, area={})",
                                trk.id, trk.label, trk.score, enriched.area
                            ),
                        );
                    }
                }
            }

            if suppressed_fp {
                continue;
            }

            if cam_rt_flags.alerts
                && !camera_suppressed_in_profile
                && !enriched.newly_entered_zones.is_empty()
            {
                for entered_name in &enriched.newly_entered_zones {
                    if let Some(line_cfg) = cam_cfg.lines.get(entered_name)
                        && line_cfg.siren
                        && (line_cfg.objects.is_empty()
                            || line_cfg.objects.iter().any(|o| o == &trk.label))
                    {
                        self.spawn_camera_siren_for_trigger(
                            camera,
                            cam_cfg,
                            &format!("line:{entered_name}"),
                            &trk.label,
                            now,
                        )
                        .await;
                    } else if let Some(zone_cfg) = cam_cfg.zones.get(entered_name)
                        && zone_cfg.siren
                        && (zone_cfg.objects.is_empty()
                            || zone_cfg.objects.iter().any(|o| o == &trk.label))
                    {
                        self.spawn_camera_siren_for_trigger(
                            camera,
                            cam_cfg,
                            &format!("zone:{entered_name}"),
                            &trk.label,
                            now,
                        )
                        .await;
                    }
                }
            }

            // Update or open ReviewSegment if enabled
            let track_sev = enriched.resolve_severity(
                trk,
                cam_cfg,
                &active_profile,
                camera_suppressed_in_profile,
                cam_rt_flags.alerts,
                false,
            );
            if !should_include_track_in_review(
                cam_cfg,
                trk,
                track_sev,
                active_review.is_some(),
                enriched.is_new,
                &enriched.newly_entered_zones,
            ) {
                continue;
            }

            let is_alert = track_sev == "alert";
            let rev = active_review.get_or_insert_with(|| {
                let rev_id = format!("{now:.6}-{}", random_suffix());
                let thumb_rel =
                    format!("/media/ninjanvr/clips/review/thumb-{safe_cam_file}-{rev_id}.webp");
                ActiveReviewState {
                    id: rev_id,
                    start_time: now,
                    last_activity_time: now,
                    last_backup_time: now,
                    severity: track_sev.to_string(),
                    thumb_path: thumb_rel,
                    thumb_time: now,
                    best_score: 0.0,
                    detections: Vec::new(),
                    objects: Vec::new(),
                    sub_labels: Vec::new(),
                    sub_label_score: enriched.effective_sub_label_score,
                    detected_attributes: Vec::new(),
                    zones: Vec::new(),
                    provenance: Some(trk.provenance.clone()),
                    is_new: true,
                    dirty: true,
                    prev_dto: None,
                }
            });

            if rev.absorb_track(trk, &enriched, is_alert, now) {
                let frame_for_rev = frame.clone();
                let bbox_for_rev = trk.bbox;
                let draw_bbox_rev = cam_cfg.snapshots.bounding_box;
                let mask_for_rev = normalized_priv_mask.clone();
                let media_root_for_rev = self.media_dir.clone();
                let review_dir_for_rev = review_dir.clone();
                let safe_cam_for_rev = safe_cam_file.clone();
                let safe_rev_id = sanitize_filename_component(&rev.id);
                let _ = tokio::task::spawn_blocking(move || {
                    let rev_boxes: &[(BoundingBox, [u8; 3])] = if draw_bbox_rev {
                        &[(bbox_for_rev, [31, 119, 180])]
                    } else {
                        &[]
                    };
                    let mut rev_img =
                        yuv420p_to_rgb_image_scaled(&frame_for_rev, None, rev_boxes, Some(180));
                    apply_privacy_masks_from_value(&mut rev_img, &mask_for_rev);
                    if let Ok(webp) = encode_image_webp(&rev_img) {
                        let fname = format!("thumb-{safe_cam_for_rev}-{safe_rev_id}.webp");
                        if let Err(err) = write_contained_media_file(
                            &media_root_for_rev,
                            &review_dir_for_rev,
                            &fname,
                            &webp,
                        ) {
                            warn!("Failed to write review thumbnail {fname}: {err:#}");
                        }
                    }
                })
                .await;
            }
        }

        // Finalize ended tracks
        for trk in ended_tracks {
            let removed_snap = track_snapshots.remove(&trk.id);
            let mut suppressed_fp = removed_snap.as_ref().is_some_and(|s| s.suppressed_fp);
            let mut matched_fp_sig_id = removed_snap.as_ref().and_then(|s| s.matched_fp_sig_id);
            if !suppressed_fp {
                let db_for_end_fp = self.db.clone();
                let trk_id_for_end_fp = trk.id.clone();
                if let Ok(Ok(Some(ev_row))) =
                    tokio::task::spawn_blocking(move || db_for_end_fp.get_event(&trk_id_for_end_fp))
                        .await
                    && ev_row["false_positive"].as_bool() == Some(true)
                {
                    suppressed_fp = true;
                    if matched_fp_sig_id.is_none() {
                        matched_fp_sig_id = ev_row["data"]["suppressed_by_fp_signature"].as_i64();
                    }
                }
            }
            if suppressed_fp {
                self.prune_false_positive_track_from_review(active_review, &trk.id, camera, now)
                    .await;
            }
            let lpr_consensus = if lpr_cfg.enabled {
                self.detector.finalize_lpr_track(&trk.id, &lpr_cfg)
            } else {
                None
            };
            let enriched = enrich_tracked_object(TrackEnrichmentInput {
                trk,
                prev: removed_snap.as_ref(),
                lpr_consensus: lpr_consensus.as_ref(),
                face_match: None,
                frame,
                cam_cfg,
                active_profile: &active_profile,
                camera_suppressed_in_profile,
                alerts_enabled: cam_rt_flags.alerts,
                suppressed_fp,
                is_ended: true,
            });
            let ev_dto = build_track_event_dto(BuildTrackEventDtoParams {
                camera,
                trk,
                enriched: &enriched,
                record_active,
                snapshots_active,
                suppressed_fp,
                matched_fp_sig_id,
                severity: enriched.severity,
                thumbnail: None,
                end_time: Some(trk.last_seen_time),
                night_mode: None,
            });

            let db_for_end = self.db.clone();
            let ev_dto_for_end = ev_dto.clone();
            let sub_label_for_end = enriched.effective_sub_label.clone();
            let sub_score_for_end = enriched.effective_sub_label_score.map(|v| v as f64);
            let trk_id_for_end = trk.id.clone();
            let cam_for_end = camera.to_string();
            let trk_label_for_end = trk.label.clone();
            let trk_score_for_end = trk.score;
            let norm_box = enriched.norm_box;
            let norm_reg = enriched.norm_reg;
            let best_score = enriched.best_score;

            let ended_event_persisted = match tokio::task::spawn_blocking(move || {
                if let Err(err) = db_for_end.upsert_event(&ev_dto_for_end) {
                    warn!(
                        "Failed to upsert ended event {} for {}: {err:#}",
                        trk_id_for_end, cam_for_end
                    );
                    return false;
                }
                if let Some(ref sl) = sub_label_for_end
                    && let Err(err) = db_for_end.set_event_sub_label_with_score(
                        &trk_id_for_end,
                        Some(sl),
                        sub_score_for_end,
                    )
                {
                    warn!(
                        "Failed to persist sub_label for ended event {} on {}: {err:#}",
                        trk_id_for_end, cam_for_end
                    );
                }
                if let Err(err) = db_for_end.insert_timeline_entry(
                    now,
                    &cam_for_end,
                    "tracked_object",
                    Some(&trk_id_for_end),
                    "gone",
                    &json!({
                        "box": norm_box,
                        "label": trk_label_for_end,
                        "region": norm_reg,
                        "score": trk_score_for_end,
                        "top_score": best_score
                    }),
                ) {
                    warn!(
                        "Failed to insert gone timeline entry for {} on {}: {err:#}",
                        trk_id_for_end, cam_for_end
                    );
                }
                true
            })
            .await
            {
                Ok(ok) => ok,
                Err(join_err) => {
                    warn!(
                        "Blocking task failed persisting ended event {} on {camera}: {join_err:#}",
                        trk.id
                    );
                    false
                }
            };

            if ended_event_persisted {
                self.comms.publish_json(
                    "events",
                    &json!({
                        "type": "end",
                        "before": ev_dto,
                        "after": ev_dto
                    }),
                );
            }
        }

        // Flush or close active review segment with {type, before, after} schema
        if let Some(rev) = active_review {
            let decision = evaluate_review_cutoff_and_checkpoint(rev, cam_cfg, active_tracks, now);

            if decision.should_flush {
                let dto = rev.to_dto(camera, decision.end_time);
                let db_for_rev = self.db.clone();
                let dto_for_rev = dto.clone();
                let rev_id_for_log = rev.id.clone();
                let cam_for_rev = camera.to_string();

                let review_persisted = match tokio::task::spawn_blocking(move || {
                    if let Err(err) = db_for_rev.insert_review_segment(&dto_for_rev) {
                        warn!(
                            "Failed to persist review segment {} for {}: {err:#}",
                            rev_id_for_log, cam_for_rev
                        );
                        return false;
                    }
                    true
                })
                .await
                {
                    Ok(ok) => ok,
                    Err(join_err) => {
                        warn!(
                            "Blocking task failed persisting review segment {} on {camera}: {join_err:#}",
                            rev.id
                        );
                        false
                    }
                };

                if review_persisted {
                    let before_dto = rev.prev_dto.clone().unwrap_or_else(|| dto.clone());
                    self.comms.publish_json(
                        "reviews",
                        &json!({
                            "type": decision.ws_msg_type,
                            "before": before_dto,
                            "after": dto
                        }),
                    );

                    rev.is_new = false;
                    rev.dirty = false;
                    rev.prev_dto = Some(dto);
                }

                if decision.should_advance_backup_time {
                    rev.last_backup_time = now;
                    let bak_cfg = &cam_cfg.record.alert_backup;
                    if decision.should_spawn_alert_backup
                        && let Some(ref target_dir) = bak_cfg.target_dir
                        && let Ok(bak_permit) =
                            self.alert_backup_semaphore.clone().try_acquire_owned()
                    {
                        let db_clone = self.db.clone();
                        let media_clone = self.media_dir.clone();
                        let target_clone = target_dir.clone();
                        let cam_owned = camera.to_string();
                        let lbl_owned = rev
                            .objects
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "alert".to_string());
                        let s_ts = rev.start_time;
                        let e_ts = rev.last_activity_time;
                        let thumb_owned = rev.thumb_path.clone();
                        let inc_snap = bak_cfg.include_snapshot;
                        tokio::spawn(async move {
                            let _permit = bak_permit;
                            if let Err(err) = backup_alert_clip(
                                &db_clone,
                                &media_clone,
                                &target_clone,
                                &cam_owned,
                                &lbl_owned,
                                s_ts,
                                e_ts,
                                Some(&thumb_owned),
                                inc_snap,
                            )
                            .await
                            {
                                warn!("Alert clip backup failed for {cam_owned}: {err:#}");
                            }
                        });
                    }
                }
                if decision.is_expired {
                    *active_review = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_vision_config_delta_flags_and_applies_only_live_editable_sections() {
        let current = CameraConfig::default();
        let mut next = current.clone();
        assert!(!diff_live_vision_config(&current, &next).any());

        next.motion.threshold = current.motion.threshold + 15;
        next.privacy_mask = json!(["0.1,0.1,0.3,0.1,0.3,0.3,0.1,0.3"]);
        next.objects.mask = json!("0.0,0.5,0.5,0.5,0.5,0.7,0.0,0.7");
        next.state_rois.insert(
            "gate".to_string(),
            ninjanvr_config::StateRoiConfig {
                crop: [0.2, 0.2, 0.6, 0.6],
                classes: vec!["closed".to_string(), "open".to_string()],
                interval_sec: 5,
                hysteresis: 3,
                anomaly_state: Some("open".to_string()),
                anomaly_timeout_sec: 300,
            },
        );
        // Decoder settings are not live-editable and must not be copied.
        next.detect.width = current.detect.width + 320;
        next.detect.fps = current.detect.fps + 5;

        let delta = diff_live_vision_config(&current, &next);
        assert_eq!(
            delta,
            LiveVisionConfigDelta {
                motion: true,
                state_rois: true,
                regions: true,
                lines: false,
                privacy_mask: true,
            }
        );

        let mut applied = current.clone();
        apply_live_vision_config(&mut applied, &next, &delta);
        assert_eq!(applied.motion.threshold, next.motion.threshold);
        assert_eq!(applied.privacy_mask, next.privacy_mask);
        assert_eq!(applied.objects.mask, next.objects.mask);
        assert!(applied.state_rois.contains_key("gate"));
        assert_eq!(applied.detect.width, current.detect.width);
        assert_eq!(applied.detect.fps, current.detect.fps);
        assert!(!diff_live_vision_config(&applied, &next).any());
    }
    use ninjanvr_comms::CommsHub;
    use ninjanvr_config::{ProfileOverrideConfig, SnapshotSelectionPolicy};
    use ninjanvr_db::ReviewFilter;
    use ninjanvr_media::new_activity_ledger;
    use ninjanvr_track::{TrackedAttribute, TriggerProvenance, compute_attribute_quality};
    use ninjanvr_vision::BoundingBox;

    fn sample_track() -> TrackedObject {
        TrackedObject {
            id: "1730500000.000000-abc123".to_string(),
            label: "person".to_string(),
            bbox: BoundingBox {
                x_min: 10,
                y_min: 10,
                x_max: 100,
                y_max: 200,
            },
            region: BoundingBox {
                x_min: 0,
                y_min: 0,
                x_max: 320,
                y_max: 320,
            },
            score: 0.91,
            top_score: 0.91,
            start_time: 1000.0,
            last_seen_time: 1001.0,
            frames_tracked: 5,
            disappeared_frames: 0,
            motionless_count: 0,
            stationary: false,
            zones: Vec::new(),
            entered_zones: Vec::new(),
            crossed_lines: Vec::new(),
            prev_bottom_center: (55.0, 200.0),
            has_snapshot: true,
            vx: 0.0,
            vy: 0.0,
            provenance: TriggerProvenance {
                trigger_ts: 1000.0,
                bbox: [10, 10, 100, 200],
                norm_bbox: [0.01, 0.01, 0.1, 0.2],
                area_px: 17100,
                area_pct: 1.5,
                aspect_ratio: 0.47,
                score: 0.91,
                top_score: 0.91,
                min_score_cfg: 0.5,
                threshold_cfg: 0.7,
                zones: Vec::new(),
                frames_tracked: 5,
                stationary: false,
            },
            attributes: Vec::new(),
            best_attribute_score: 0.0,
            inferred_sub_label: None,
        }
    }

    fn build_isolated_pipeline(tag: &str) -> (VisionPipeline, PathBuf) {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr-pipeline-{tag}-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let db_path = tmp_dir.join("ninjanvr.db");
        let media_dir = tmp_dir.join("media");
        std::fs::create_dir_all(&media_dir).unwrap();

        let cfg = NinjaConfig::from_yaml_str(
            "version: 0.18-0\ndetectors:\n  cpu:\n    type: openvino\n    device: CPU\n",
            tmp_dir.join("config.yml"),
        )
        .unwrap();
        let db = NinjaDatabase::open(&db_path).unwrap();
        let comms = Arc::new(CommsHub::new());
        let detector = Arc::new(AiDetectorEngine::new(&cfg));

        let pipeline = VisionPipeline {
            config: Arc::new(RwLock::new(cfg)),
            db,
            comms,
            detector,
            activity_ledger: new_activity_ledger(),
            latest_frames: Arc::new(RwLock::new(HashMap::new())),
            latest_yuv_frames: Arc::new(RwLock::new(HashMap::new())),
            preview_frames: crate::preview_frames::PreviewFrameStore::new(&media_dir),
            camera_metrics: Arc::new(RwLock::new(HashMap::new())),
            media_dir,
            log_hub: crate::LogBufferHub::new(),
            ptz_active_presets: Arc::new(RwLock::new(HashMap::new())),
            camera_driver_runtime: crate::camera_protocols::CameraDriverRuntime::new(),
            snapshot_http_client: build_snapshot_http_client(),
            high_res_snapshot_semaphore: Arc::new(Semaphore::new(
                MAX_CONCURRENT_HIGH_RES_SNAPSHOT_TASKS,
            )),
            genai_verify_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_GENAI_VERIFY_TASKS)),
            alert_backup_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_ALERT_BACKUP_TASKS)),
        };
        (pipeline, tmp_dir)
    }

    fn drain_ws_messages(rx: &mut tokio::sync::broadcast::Receiver<String>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(raw) = rx.try_recv() {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                out.push(v);
            }
        }
        out
    }

    #[test]
    fn test_profile_alert_suppression_rules() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.review.alerts.enabled = true;
        cam_cfg.review.alerts.labels = vec!["person".to_string(), "car".to_string()];
        let mut trk = sample_track();

        // ARMED promotes matching track to alert
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk, "armed", false),
            "alert"
        );

        // DISARMED suppresses alert promotion across all cameras
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk, "disarmed", false),
            "detection"
        );

        // HOME suppresses indoor cameras while perimeter cameras stay armed
        cam_cfg.indoor = false;
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk, "home", false),
            "alert"
        );
        cam_cfg.indoor = true;
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk, "home", false),
            "detection"
        );

        // Custom profile disabled_alerts_cameras: ["front_door"] and ["*"] downgrade alerts to "detection"
        cam_cfg.indoor = false;
        let mut cfg =
            NinjaConfig::from_yaml_str("version: 0.18-0\n", PathBuf::from("/tmp/c.yml")).unwrap();
        cfg.profiles.insert(
            "quiet_front".to_string(),
            ProfileOverrideConfig {
                disabled_alerts_cameras: vec!["front_door".to_string()],
                disabled_record_cameras: Vec::new(),
            },
        );
        cfg.profiles.insert(
            "silent_all".to_string(),
            ProfileOverrideConfig {
                disabled_alerts_cameras: vec!["*".to_string()],
                disabled_record_cameras: Vec::new(),
            },
        );
        assert!(is_camera_alert_suppressed_in_profile(
            &cfg,
            "quiet_front",
            "front_door"
        ));
        assert!(!is_camera_alert_suppressed_in_profile(
            &cfg,
            "quiet_front",
            "back_yard"
        ));
        assert!(is_camera_alert_suppressed_in_profile(
            &cfg,
            "silent_all",
            "back_yard"
        ));
        assert_eq!(
            compute_track_severity_for_profile(
                &cam_cfg,
                &trk,
                "quiet_front",
                is_camera_alert_suppressed_in_profile(&cfg, "quiet_front", "front_door")
            ),
            "detection"
        );
        assert_eq!(
            compute_track_severity_for_profile(
                &cam_cfg,
                &trk,
                "silent_all",
                is_camera_alert_suppressed_in_profile(&cfg, "silent_all", "back_yard")
            ),
            "detection"
        );

        // required_zones and labels rules for alerts and detections
        cam_cfg.review.alerts.required_zones = vec!["driveway".to_string()];
        // Track has not entered "driveway" yet -> downgraded to "detection"
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk, "armed", false),
            "detection"
        );
        // Track enters "driveway" -> promoted to "alert"
        trk.entered_zones = vec!["driveway".to_string()];
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk, "armed", false),
            "alert"
        );
        // Label not in alerts.labels ("cat") -> downgraded to "detection" even inside required_zones
        trk.label = "cat".to_string();
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk, "armed", false),
            "detection"
        );
    }

    #[test]
    fn dominant_color_and_night_ir_helpers_classify_synthetic_yuv420p_frames() {
        let w = 64u32;
        let h = 64u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let bbox = BoundingBox::new(8, 8, 56, 56);

        // Monochrome IR night frame (U=128, V=128) vs saturated color frame
        let mut ir_frame = vec![100u8; y_len + uv_len * 2];
        ir_frame[y_len..].fill(128);
        assert!(is_frame_night_ir(&ir_frame, w, h));

        let mut color_frame = vec![120u8; y_len + uv_len * 2];
        color_frame[y_len..y_len + uv_len].fill(90);
        color_frame[y_len + uv_len..].fill(210);
        assert!(!is_frame_night_ir(&color_frame, w, h));

        // Dominant color tags: red, blue, white, black, neutral gray
        let mut red = vec![90u8; y_len + uv_len * 2];
        red[y_len..y_len + uv_len].fill(90);
        red[y_len + uv_len..].fill(240);
        assert_eq!(classify_dominant_color_tag(&red, w, h, bbox), "color:red");

        let mut blue = vec![60u8; y_len + uv_len * 2];
        blue[y_len..y_len + uv_len].fill(240);
        blue[y_len + uv_len..].fill(110);
        assert_eq!(classify_dominant_color_tag(&blue, w, h, bbox), "color:blue");

        let mut white = vec![235u8; y_len + uv_len * 2];
        white[y_len..].fill(128);
        assert_eq!(
            classify_dominant_color_tag(&white, w, h, bbox),
            "color:white"
        );

        let mut black = vec![20u8; y_len + uv_len * 2];
        black[y_len..].fill(128);
        assert_eq!(
            classify_dominant_color_tag(&black, w, h, bbox),
            "color:black"
        );

        let gray = vec![128u8; y_len + uv_len * 2];
        assert_eq!(classify_dominant_color_tag(&gray, w, h, bbox), "color:gray");
    }

    #[tokio::test]
    async fn evaluate_camera_tamper_triggers_after_sustained_luma_drop_and_clears_on_recovery() {
        let (pipeline, tmp_dir) = build_isolated_pipeline("tamper");
        let mut ws_rx = pipeline.comms.subscribe_ws();
        let w = 320u32;
        let h = 180u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;

        // Structured normal scene
        let mut normal_scene = vec![128u8; y_len + uv_len * 2];
        for (i, px) in normal_scene.iter_mut().enumerate().take(y_len) {
            *px = ((i * 19) % 100 + 25) as u8;
        }
        // Occluded / blind luma-drop frame
        let occluded_scene = vec![12u8; y_len + uv_len * 2];

        let mut consecutive = 0u32;
        let threshold = 4u32;

        // Normal frame keeps counter at 0
        assert!(
            !pipeline
                .evaluate_camera_tamper(
                    "front_door",
                    &normal_scene,
                    w,
                    h,
                    1000.0,
                    &mut consecutive,
                    threshold
                )
                .await
        );
        assert_eq!(consecutive, 0);

        // Frames 1..3 of sustained luma drop increment counter without triggering yet
        for i in 1..threshold {
            assert!(
                !pipeline
                    .evaluate_camera_tamper(
                        "front_door",
                        &occluded_scene,
                        w,
                        h,
                        1000.0 + i as f64,
                        &mut consecutive,
                        threshold
                    )
                    .await
            );
            assert_eq!(consecutive, i);
        }

        // Frame 4 reaches threshold: triggers tamper alert, broadcasts ON, and persists reviewsegment
        assert!(
            pipeline
                .evaluate_camera_tamper(
                    "front_door",
                    &occluded_scene,
                    w,
                    h,
                    1004.0,
                    &mut consecutive,
                    threshold
                )
                .await
        );
        assert_eq!(consecutive, threshold);

        let msgs = drain_ws_messages(&mut ws_rx);
        assert!(
            msgs.iter()
                .any(|m| m["topic"] == "front_door/tamper" && m["payload"] == "ON")
        );

        let reviews = pipeline.db.query_reviews(&ReviewFilter::default()).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].severity, "alert");
        assert_eq!(reviews[0].data["objects"], json!(["camera_tamper"]));

        // Scene restoration clears tamper state and broadcasts OFF
        assert!(
            !pipeline
                .evaluate_camera_tamper(
                    "front_door",
                    &normal_scene,
                    w,
                    h,
                    1005.0,
                    &mut consecutive,
                    threshold
                )
                .await
        );
        assert_eq!(consecutive, 0);
        let recovery_msgs = drain_ws_messages(&mut ws_rx);
        assert!(
            recovery_msgs
                .iter()
                .any(|m| m["topic"] == "front_door/tamper" && m["payload"] == "OFF")
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn process_lifecycle_events_covers_activation_privacy_mask_upgrade_and_termination() {
        let (pipeline, tmp_dir) = build_isolated_pipeline("lifecycle");
        let mut ws_rx = pipeline.comms.subscribe_ws();
        let camera = "front_door";

        let mut cam_cfg = CameraConfig::default();
        cam_cfg.snapshots.enabled = true;
        cam_cfg.snapshots.high_res = false;
        cam_cfg.snapshots.from_main_stream = false;
        cam_cfg.snapshots.selection = SnapshotSelectionPolicy::BestInZone;
        cam_cfg.record.enabled = true;
        cam_cfg.review.alerts.enabled = true;
        cam_cfg.review.alerts.labels = vec!["person".to_string()];
        cam_cfg.review.alerts.cutoff_time = 20;
        // Configure privacy mask covering top-left quadrant (0.0..0.4 x 0.0..0.4)
        cam_cfg.privacy_mask = json!(["0.0,0.0,0.4,0.0,0.4,0.4,0.0,0.4"]);

        let w = 320u32;
        let h = 180u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        // Bright white frame (Y=235, U=128, V=128) so privacy mask redaction to (12, 14, 18) is unmistakable
        let mut frame_bytes = vec![235u8; y_len + uv_len * 2];
        frame_bytes[y_len..].fill(128);
        let yuv_frame = YuvFrame {
            camera: Arc::from(camera),
            timestamp: 1000.0,
            width: w,
            height: h,
            data: Arc::new(frame_bytes),
        };

        let mut active_review: Option<ActiveReviewState> = None;
        let mut track_snapshots: HashMap<String, TrackPersistSnapshot> = HashMap::new();

        // 1. Track Activation & Snapshot Generation (score 0.82)
        let mut trk = sample_track();
        trk.id = "1730500000.000000-trk01".to_string();
        trk.score = 0.82;
        trk.top_score = 0.82;
        trk.start_time = 1000.0;
        trk.last_seen_time = 1000.0;
        trk.bbox = BoundingBox::new(150, 80, 220, 170);

        let ctx1 = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 1000.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx1,
                std::slice::from_ref(&trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;

        // Verify event row in SQLite
        let ev1 = pipeline
            .db
            .get_event(&trk.id)
            .unwrap()
            .expect("event row must be created");
        assert!(ev1["end_time"].is_null());
        assert_eq!(ev1["has_snapshot"], json!(true));
        assert_eq!(ev1["has_clip"], json!(true));
        assert!((ev1["top_score"].as_f64().unwrap() - 0.82).abs() < 1e-4);

        // Verify ActiveReviewState and on-disk snapshots with privacy mask redaction
        let rev_id = active_review
            .as_ref()
            .expect("ActiveReviewState must be opened")
            .id
            .clone();
        let clips_dir = pipeline.media_dir.join("clips");
        let jpg_path = clips_dir.join(format!("{}.jpg", trk.id));
        let webp_path = clips_dir.join(format!("{camera}-{}.webp", trk.id));
        let rev_thumb_path = clips_dir
            .join("review")
            .join(format!("thumb-{camera}-{rev_id}.webp"));
        assert!(jpg_path.exists(), "snapshot JPEG must exist on disk");
        assert!(webp_path.exists(), "snapshot WebP must exist on disk");
        assert!(
            rev_thumb_path.exists(),
            "review thumbnail WebP must exist on disk"
        );

        let decoded_webp = image::open(&webp_path).unwrap().to_rgb8();
        // Inside privacy mask at (20, 20): redacted to [12, 14, 18]
        assert_eq!(*decoded_webp.get_pixel(20, 20), image::Rgb([12, 14, 18]));
        // Outside privacy mask at (280, 150): bright white (> 200)
        assert!(decoded_webp.get_pixel(280, 150)[0] > 200);

        // Verify WebSocket "new" event and "visible" timeline entry
        let ws_msgs1 = drain_ws_messages(&mut ws_rx);
        assert!(ws_msgs1.iter().any(|m| {
            m["topic"] == "events"
                && serde_json::from_str::<Value>(m["payload"].as_str().unwrap_or("{}"))
                    .is_ok_and(|p| p["type"] == "new" && p["after"]["id"] == trk.id)
        }));
        let timeline1 = pipeline
            .db
            .query_timeline(Some(camera), Some(&trk.id), 20)
            .unwrap();
        assert!(timeline1.iter().any(|t| t["class_type"] == "visible"));

        // 2. Snapshot Upgrade Policy (BestInZone) & Zone Entry Timeline Event (score 0.94 in "driveway")
        trk.score = 0.94;
        trk.top_score = 0.94;
        trk.last_seen_time = 1002.0;
        trk.zones = vec!["driveway".to_string()];
        trk.entered_zones = vec!["driveway".to_string()];

        let ctx2 = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 1002.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx2,
                std::slice::from_ref(&trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;

        let ev2 = pipeline.db.get_event(&trk.id).unwrap().unwrap();
        assert!((ev2["top_score"].as_f64().unwrap() - 0.94).abs() < 1e-4);
        assert_eq!(ev2["zones"], json!(["driveway"]));

        let timeline2 = pipeline
            .db
            .query_timeline(Some(camera), Some(&trk.id), 20)
            .unwrap();
        assert!(
            timeline2.iter().any(|t| t["class_type"] == "entered_zone"),
            "entering a zone must record class_type = entered_zone in timeline"
        );
        let ws_msgs2 = drain_ws_messages(&mut ws_rx);
        assert!(ws_msgs2.iter().any(|m| {
            m["topic"] == "events"
                && serde_json::from_str::<Value>(m["payload"].as_str().unwrap_or("{}"))
                    .is_ok_and(|p| p["type"] == "update")
        }));

        // Verify BestInZone rejects snapshot overwrite when object leaves zone, whereas BestOverall accepts it
        std::fs::write(&webp_path, b"sentinel").unwrap();
        trk.score = 0.99;
        trk.top_score = 0.99;
        trk.zones.clear(); // stepped out of "driveway" while entered_zones still has ["driveway"]
        let ctx_out_of_zone = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 1003.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx_out_of_zone,
                std::slice::from_ref(&trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        assert_eq!(
            std::fs::read(&webp_path).unwrap(),
            b"sentinel",
            "BestInZone must not overwrite snapshot when out of zone"
        );

        // Switch policy to FirstTrigger: even a new score surge inside zone must not overwrite sentinel
        let mut cam_cfg_first = cam_cfg.clone();
        cam_cfg_first.snapshots.selection = SnapshotSelectionPolicy::FirstTrigger;
        track_snapshots.get_mut(&trk.id).unwrap().best_score = 0.70;
        trk.zones = vec!["driveway".to_string()];
        let ctx_first = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg_first,
            frame: &yuv_frame,
            now: 1003.5,
        };
        pipeline
            .process_lifecycle_events(
                &ctx_first,
                std::slice::from_ref(&trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        assert_eq!(
            std::fs::read(&webp_path).unwrap(),
            b"sentinel",
            "FirstTrigger must never overwrite initial snapshot"
        );

        // Switch policy to BestOverall: score improvement overwrites sentinel with a valid WebP
        let mut cam_cfg_overall = cam_cfg.clone();
        cam_cfg_overall.snapshots.selection = SnapshotSelectionPolicy::BestOverall;
        track_snapshots.get_mut(&trk.id).unwrap().best_score = 0.70;
        trk.zones.clear();
        let ctx_overall = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg_overall,
            frame: &yuv_frame,
            now: 1004.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx_overall,
                std::slice::from_ref(&trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        assert!(
            std::fs::read(&webp_path).unwrap().starts_with(b"RIFF"),
            "BestOverall must overwrite snapshot when score improves even out of zone"
        );

        // 3. Track Termination & Review Finalization (ended_tracks + advance past cutoff_time)
        let _ = drain_ws_messages(&mut ws_rx);
        trk.last_seen_time = 1005.0;
        let ctx_end = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 1030.0, // > 1004.0 + 20s cutoff
        };
        pipeline
            .process_lifecycle_events(
                &ctx_end,
                &[],
                std::slice::from_ref(&trk),
                &mut active_review,
                &mut track_snapshots,
            )
            .await;

        assert!(
            active_review.is_none(),
            "ActiveReviewState must be finalized and cleared past cutoff"
        );
        let ev_end = pipeline.db.get_event(&trk.id).unwrap().unwrap();
        assert_eq!(ev_end["end_time"], json!(1005.0));

        let finalized_rev = pipeline
            .db
            .get_review_by_id(&rev_id)
            .unwrap()
            .expect("reviewsegment must be persisted");
        assert_eq!(finalized_rev.severity, "alert");
        assert!(finalized_rev.end_time.is_some());

        let ws_msgs_end = drain_ws_messages(&mut ws_rx);
        assert!(ws_msgs_end.iter().any(|m| {
            m["topic"] == "events"
                && serde_json::from_str::<Value>(m["payload"].as_str().unwrap_or("{}"))
                    .is_ok_and(|p| p["type"] == "end" && p["after"]["id"] == trk.id)
        }));
        assert!(ws_msgs_end.iter().any(|m| {
            m["topic"] == "reviews"
                && serde_json::from_str::<Value>(m["payload"].as_str().unwrap_or("{}"))
                    .is_ok_and(|p| p["type"] == "end" && p["after"]["id"] == rev_id)
        }));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn lifecycle_persists_clean_snapshot_inferred_sub_label_and_detected_attributes() {
        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let (pipeline, tmp_dir) = build_isolated_pipeline("clean-snap-attrs");
        let camera = "front_door";

        let mut cam_cfg = CameraConfig::default();
        cam_cfg.snapshots.enabled = true;
        cam_cfg.snapshots.high_res = false;
        cam_cfg.snapshots.from_main_stream = false;
        cam_cfg.snapshots.selection = SnapshotSelectionPolicy::BestAttribute;
        cam_cfg.record.enabled = true;

        let w = 320u32;
        let h = 180u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let mut frame_bytes = vec![200u8; y_len + uv_len * 2];
        frame_bytes[y_len..].fill(128);
        let yuv_frame = YuvFrame {
            camera: Arc::from(camera),
            timestamp: 2000.0,
            width: w,
            height: h,
            data: Arc::new(frame_bytes),
        };

        let mut active_review: Option<ActiveReviewState> = None;
        let mut track_snapshots: HashMap<String, TrackPersistSnapshot> = HashMap::new();

        let mut trk = sample_track();
        trk.id = "1730500999.000000-attr01".to_string();
        trk.label = "car".to_string();
        trk.score = 0.86;
        trk.top_score = 0.86;
        trk.bbox = BoundingBox::new(40, 40, 280, 160);
        trk.inferred_sub_label = Some("correios".to_string());
        trk.best_attribute_score = compute_attribute_quality(0.93, 60 * 24, w, h);
        trk.attributes = vec![
            TrackedAttribute {
                label: "license_plate".to_string(),
                score: 0.93,
                bbox: BoundingBox::new(120, 110, 180, 134),
                norm_bbox: BoundingBox::new(120, 110, 180, 134).to_normalized(w, h),
                area_px: 60 * 24,
            },
            TrackedAttribute {
                label: "correios".to_string(),
                score: 0.89,
                bbox: BoundingBox::new(80, 60, 160, 100),
                norm_bbox: BoundingBox::new(80, 60, 160, 100).to_normalized(w, h),
                area_px: 80 * 40,
            },
        ];

        let ctx = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 2000.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx,
                std::slice::from_ref(&trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;

        // 1. Assert SQLite event.sub_label and event.data.detected_attributes
        let ev = pipeline
            .db
            .get_event(&trk.id)
            .unwrap()
            .expect("event row must be persisted");
        assert_eq!(ev["sub_label"], json!("correios"));
        let det_attrs = ev["data"]["detected_attributes"]
            .as_array()
            .expect("detected_attributes array must exist");
        assert_eq!(det_attrs.len(), 2);
        assert!(
            det_attrs
                .iter()
                .any(|a| a["label"] == "license_plate" && a["area_px"] == 1440)
        );
        assert!(
            det_attrs
                .iter()
                .any(|a| a["label"] == "correios" && a["area_px"] == 3200)
        );
        assert!(ev["data"]["license_plate_box"].is_array());

        // 2. Assert {clips_dir}/{camera}-{id}-clean.webp exists on disk and is unannotated
        let clean_webp_path = pipeline
            .media_dir
            .join("clips")
            .join(format!("{camera}-{}-clean.webp", trk.id));
        assert!(
            clean_webp_path.exists(),
            "clean unannotated WebP snapshot must be persisted on disk"
        );
        let clean_bytes = std::fs::read(&clean_webp_path).unwrap();
        assert!(clean_bytes.starts_with(b"RIFF") && &clean_bytes[8..12] == b"WEBP");

        // 2b. Verify attribute-guided snapshot replacement when parent score is flat (0.86 -> 0.85)
        //     but license_plate area increases (60x24 -> 140x50)
        std::fs::write(&clean_webp_path, b"sentinel_old_clean").unwrap();
        trk.score = 0.85;
        trk.last_seen_time = 2001.0;
        trk.best_attribute_score = compute_attribute_quality(0.95, 140 * 50, w, h);
        trk.attributes[0].score = 0.95;
        trk.attributes[0].area_px = 140 * 50;
        let ctx2 = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 2001.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx2,
                std::slice::from_ref(&trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        let upgraded_clean = std::fs::read(&clean_webp_path).unwrap();
        assert!(
            upgraded_clean.starts_with(b"RIFF") && &upgraded_clean[8..12] == b"WEBP",
            "improved attribute quality must overwrite clean snapshot even when parent score decreased"
        );

        // 2c. Finalize track in ended_tracks and verify sub_label + detected_attributes remain persisted
        trk.last_seen_time = 2003.0;
        let ctx_end = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 2003.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx_end,
                &[],
                std::slice::from_ref(&trk),
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        let ev_final = pipeline.db.get_event(&trk.id).unwrap().unwrap();
        assert_eq!(ev_final["sub_label"], json!("correios"));
        assert_eq!(ev_final["end_time"], json!(2003.0));
        assert_eq!(
            ev_final["data"]["detected_attributes"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // 3. Assert GET /api/events/{id}/snapshot-clean.webp returns 200 OK with valid WebP magic bytes
        let cfg_path = tmp_dir.join("config.yml");
        std::fs::write(&cfg_path, "version: 0.18-0\n").unwrap();
        let mut state = crate::new_app_state(
            &cfg_path,
            tmp_dir.join("web"),
            pipeline.media_dir.clone(),
            false,
        )
        .await
        .unwrap();
        state.db = pipeline.db.clone();
        let app = crate::build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/events/{}/snapshot-clean.webp", trk.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body_bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        assert!(
            body_bytes.starts_with(b"RIFF") && &body_bytes[8..12] == b"WEBP",
            "GET /api/events/{{id}}/snapshot-clean.webp must return valid WebP magic bytes"
        );

        // 4. Verify LPR multi-frame voting + known_plates -> sub_label & recognized_license_plate
        {
            let mut cfg = pipeline.config.write().await;
            cfg.lpr.enabled = true;
            cfg.lpr
                .known_plates
                .insert("ABC1D23".to_string(), "Family SUV".to_string());
        }
        let mut lpr_trk = trk.clone();
        lpr_trk.id = "1700002100.000000-lpr001".to_string();
        lpr_trk.label = "license_plate".to_string();
        lpr_trk.attributes.clear();
        for read_str in ["ABC1D23", "ABC1023", "ABC1D23"] {
            lpr_trk.inferred_sub_label = Some(read_str.to_string());
            let lpr_ctx = FrameLifecycleCtx {
                camera,
                cam_cfg: &cam_cfg,
                frame: &yuv_frame,
                now: 2100.0,
            };
            pipeline
                .process_lifecycle_events(
                    &lpr_ctx,
                    std::slice::from_ref(&lpr_trk),
                    &[],
                    &mut active_review,
                    &mut track_snapshots,
                )
                .await;
        }
        let lpr_ev = pipeline.db.get_event(&lpr_trk.id).unwrap().unwrap();
        assert_eq!(lpr_ev["data"]["recognized_license_plate"], json!("ABC1D23"));
        assert_eq!(lpr_ev["sub_label"], json!("Family SUV"));
        assert!(lpr_ev["data"]["license_plate_box"].is_array());
        assert!(
            build_snapshot_http_client().is_some(),
            "build_snapshot_http_client must install rustls provider and succeed"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn pipeline_suppresses_recurring_detection_matching_false_positive_signature() {
        let (pipeline, tmp_dir) = build_isolated_pipeline("fp_suppress");
        let camera = "patio";
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.snapshots.enabled = true;
        cam_cfg.snapshots.from_main_stream = false;
        cam_cfg.review.alerts.enabled = true;
        cam_cfg.review.alerts.labels = vec!["person".to_string()];

        let w = 320u32;
        let h = 180u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let mut yuv_bytes = vec![120u8; y_len + uv_len * 2];
        for y in 0..h as usize {
            for x in 0..w as usize {
                yuv_bytes[y * (w as usize) + x] = (30 + (x * 180) / (w as usize)) as u8;
            }
        }
        let yuv_frame = YuvFrame {
            camera: Arc::from(camera),
            timestamp: 3000.0,
            width: w,
            height: h,
            data: Arc::new(yuv_bytes),
        };

        // 1. First detection of static patio plant misclassified as "person"
        let mut active_review = None;
        let mut track_snapshots = HashMap::new();
        let mut trk1 = sample_track();
        trk1.id = "1730503000.000000-plant1".to_string();
        trk1.label = "person".to_string();
        trk1.bbox = BoundingBox::new(40, 20, 120, 150);

        let ctx1 = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 3000.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx1,
                std::slice::from_ref(&trk1),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;

        let ev1 = pipeline.db.get_event(&trk1.id).unwrap().unwrap();
        assert_eq!(ev1["false_positive"], json!(false));
        assert!(ev1["data"]["dhash64"].as_u64().is_some());
        assert!(active_review.is_some(), "unmarked track opens alert review");

        // 2. Operator marks ev1 as False Positive -> stores spatial + dhash64 signature
        pipeline
            .db
            .set_event_false_positive(&trk1.id, true)
            .unwrap();
        let sigs = pipeline
            .db
            .list_false_positive_signatures(Some(camera))
            .unwrap();
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0]["hit_count"], 1);

        // 3. Subsequent track at virtually identical bbox & visual pattern is suppressed immediately
        let mut active_review_2 = None;
        let mut trk2 = sample_track();
        trk2.id = "1730503010.000000-plant2".to_string();
        trk2.label = "person".to_string();
        trk2.bbox = BoundingBox::new(41, 21, 120, 150);

        let ctx2 = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 3010.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx2,
                std::slice::from_ref(&trk2),
                &[],
                &mut active_review_2,
                &mut track_snapshots,
            )
            .await;

        let ev2 = pipeline.db.get_event(&trk2.id).unwrap().unwrap();
        assert_eq!(ev2["false_positive"], json!(true));
        assert_eq!(ev2["data"]["max_severity"], json!("detection"));
        assert!(
            active_review_2.is_none(),
            "suppressed recurring false positive must not open an alert review segment"
        );
        let sigs_after = pipeline
            .db
            .list_false_positive_signatures(Some(camera))
            .unwrap();
        assert_eq!(sigs_after[0]["hit_count"], 2);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn compute_track_severity_for_profile_evaluates_sub_labels_and_exclude_sub_labels() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.review.alerts.enabled = true;
        cam_cfg.review.alerts.labels = vec!["person".to_string()];
        cam_cfg.review.alerts.exclude_sub_labels = vec!["Bruno".to_string()];

        // Known resident "Bruno" in exclude_sub_labels downgrades alert -> detection
        let mut trk_bruno = sample_track();
        trk_bruno.inferred_sub_label = Some("Bruno".to_string());
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk_bruno, "armed", false),
            "detection"
        );

        // Unknown person triggers alert
        let mut trk_unknown = sample_track();
        trk_unknown.inferred_sub_label = Some("unknown".to_string());
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk_unknown, "armed", false),
            "alert"
        );

        // Unclassified person (None) triggers alert when sub_labels filter is empty
        let trk_none = sample_track();
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk_none, "armed", false),
            "alert"
        );

        // When sub_labels is explicitly restricted to ["unknown"], only "unknown" triggers alert
        cam_cfg.review.alerts.sub_labels = vec!["unknown".to_string()];
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk_unknown, "armed", false),
            "alert"
        );
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk_none, "armed", false),
            "detection"
        );
    }

    #[tokio::test]
    async fn evaluate_camera_roi_states_enforces_hysteresis_and_emits_anomaly_review_segment_after_timeout()
     {
        use ninjanvr_config::StateRoiConfig;

        let (pipeline, tmp_dir) = build_isolated_pipeline("roi-state-anomaly");
        let mut ws_rx = pipeline.comms.subscribe_ws();
        let camera = "front_gate";

        let mut rois = BTreeMap::new();
        rois.insert(
            "gate".to_string(),
            StateRoiConfig {
                crop: [0.2, 0.2, 0.8, 0.8],
                classes: vec!["closed".to_string(), "open".to_string()],
                interval_sec: 5,
                hysteresis: 3,
                anomaly_state: Some("open".to_string()),
                anomaly_timeout_sec: 120,
            },
        );
        let mut monitor = CameraRoiStateMonitor::new(&rois);

        // 1. Initial baseline observation ("closed") publishes initial state
        let tr0 = pipeline
            .record_roi_state_observation(camera, &mut monitor, "gate", "closed", 0.95, 1000.0)
            .await
            .unwrap();
        assert_eq!(tr0.current_state, "closed");
        assert!(!tr0.state_changed);
        assert_eq!(
            pipeline
                .comms
                .get_camera_roi_states(camera)
                .get("gate")
                .map(String::as_str),
            Some("closed")
        );
        let init_msgs = drain_ws_messages(&mut ws_rx);
        assert!(
            init_msgs
                .iter()
                .any(|m| m["topic"] == "front_gate/state/gate" && m["payload"] == "closed")
        );

        // 2. Single-frame and 2-frame flicker to "open" must NOT transition (hysteresis = 3)
        let _ = pipeline
            .record_roi_state_observation(camera, &mut monitor, "gate", "open", 0.90, 1005.0)
            .await;
        let _ = pipeline
            .record_roi_state_observation(camera, &mut monitor, "gate", "open", 0.90, 1010.0)
            .await;
        let _ = pipeline
            .record_roi_state_observation(camera, &mut monitor, "gate", "closed", 0.95, 1015.0)
            .await;
        assert_eq!(
            pipeline
                .comms
                .get_camera_roi_states(camera)
                .get("gate")
                .map(String::as_str),
            Some("closed")
        );
        assert!(drain_ws_messages(&mut ws_rx).is_empty());

        // 3. 3 consecutive "open" frames transition CLOSED -> OPEN at t=1030.0
        let _ = pipeline
            .record_roi_state_observation(camera, &mut monitor, "gate", "open", 0.91, 1020.0)
            .await;
        let _ = pipeline
            .record_roi_state_observation(camera, &mut monitor, "gate", "open", 0.92, 1025.0)
            .await;
        let tr_open = pipeline
            .record_roi_state_observation(camera, &mut monitor, "gate", "open", 0.93, 1030.0)
            .await
            .unwrap();
        assert!(tr_open.state_changed);
        assert_eq!(tr_open.current_state, "open");
        assert!(!tr_open.anomaly_triggered);

        let open_msgs = drain_ws_messages(&mut ws_rx);
        assert!(
            open_msgs
                .iter()
                .any(|m| m["topic"] == "front_gate/state/gate" && m["payload"] == "open")
        );

        // 4. Recovery to "closed" at t=1110.0 (elapsed 80s < 120s timeout) resets anomaly timer without firing ReviewSegment
        let _ = pipeline
            .record_roi_state_observation(camera, &mut monitor, "gate", "open", 0.93, 1095.0)
            .await;
        let _ = pipeline
            .record_roi_state_observation(camera, &mut monitor, "gate", "closed", 0.94, 1100.0)
            .await;
        let _ = pipeline
            .record_roi_state_observation(camera, &mut monitor, "gate", "closed", 0.95, 1105.0)
            .await;
        let tr_rec = pipeline
            .record_roi_state_observation(camera, &mut monitor, "gate", "closed", 0.96, 1110.0)
            .await
            .unwrap();
        assert!(tr_rec.state_changed);
        assert_eq!(tr_rec.current_state, "closed");
        assert!(
            pipeline
                .db
                .query_reviews(&ReviewFilter::default())
                .unwrap()
                .is_empty(),
            "recovery before anomaly_timeout_sec must not emit a ReviewSegment alert"
        );
        let _ = drain_ws_messages(&mut ws_rx);

        // 5. Re-open at t=1200.0 ( via YUV420p frame evaluation) and sustain >= 120s -> emits ReviewSegment alert at t=1320.0
        let w = 160u32;
        let h = 120u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let mut open_yuv = vec![35u8; y_len + uv_len * 2];
        for y in 24..96 {
            for x in 32..128 {
                open_yuv[y * (w as usize) + x] = if (x + y) % 2 == 0 { 240 } else { 185 };
            }
        }

        let _ = pipeline
            .evaluate_camera_roi_states(camera, &open_yuv, w, h, 1190.0, &[], &mut monitor, true)
            .await;
        let _ = pipeline
            .evaluate_camera_roi_states(camera, &open_yuv, w, h, 1195.0, &[], &mut monitor, true)
            .await;
        let trs_open2 = pipeline
            .evaluate_camera_roi_states(camera, &open_yuv, w, h, 1200.0, &[], &mut monitor, true)
            .await;
        assert_eq!(trs_open2[0].current_state, "open");
        assert!(trs_open2[0].state_changed);

        // At t=1319.0 (119s < 120s): still no alert
        let _ = pipeline
            .evaluate_camera_roi_states(camera, &open_yuv, w, h, 1319.0, &[], &mut monitor, true)
            .await;
        assert!(
            pipeline
                .db
                .query_reviews(&ReviewFilter::default())
                .unwrap()
                .is_empty()
        );

        // At t=1320.0 (120s >= 120s): triggers ReviewSegment alert
        let trs_alert = pipeline
            .evaluate_camera_roi_states(camera, &open_yuv, w, h, 1320.0, &[], &mut monitor, true)
            .await;
        assert!(trs_alert[0].anomaly_triggered);

        let reviews = pipeline.db.query_reviews(&ReviewFilter::default()).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].severity, "alert");
        assert_eq!(reviews[0].camera, camera);
        assert_eq!(reviews[0].data["objects"], json!(["roi:gate:open"]));
        assert_eq!(reviews[0].data["metadata"]["roi_name"], json!("gate"));
        assert_eq!(reviews[0].data["metadata"]["state"], json!("open"));

        let alert_msgs = drain_ws_messages(&mut ws_rx);
        assert!(alert_msgs.iter().any(|m| m["topic"] == "reviews"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn ffmpeg_args_and_yuv_dimension_sanitizers_enforce_rtsp_whitelist_and_even_bounds() {
        assert!(is_valid_rtsp_stream_url(
            "rtsp://user:pass@192.168.1.20:554/cam/realmonitor?channel=1&subtype=1"
        ));
        assert!(is_valid_rtsp_stream_url("rtsps://10.0.0.10:322/stream1"));
        assert!(!is_valid_rtsp_stream_url("-f lavfi -i testsrc"));
        assert!(!is_valid_rtsp_stream_url("file:///etc/passwd"));
        assert!(!is_valid_rtsp_stream_url("concat:/tmp/a.mp4|/tmp/b.mp4"));
        assert!(!is_valid_rtsp_stream_url(
            "http://169.254.169.254/latest/meta-data"
        ));
        assert!(!is_valid_rtsp_stream_url("rtsp://"));
        assert!(!is_valid_rtsp_stream_url(
            "rtsp://10.0.0.1/stream with space"
        ));

        let args = build_vision_decoder_ffmpeg_args("rtsp://10.0.0.50:554/sub", 641, 361, 5)
            .expect("valid RTSP URL must produce args");
        assert!(args.contains(&"-nostdin".to_string()));
        assert!(args.contains(&"-protocol_whitelist".to_string()));
        assert!(args.contains(&"rtsp,rtsps,rtp,tcp,udp,tls,crypto".to_string()));
        assert!(args.contains(&"-skip_loop_filter:v".to_string()));
        assert!(args.contains(&"nokey".to_string()));
        assert!(args.contains(&"-an".to_string()));
        assert!(args.contains(&"-sws_flags".to_string()));
        assert!(args.contains(&"fast_bilinear".to_string()));
        assert!(args.contains(&"fps=5,scale=640:360".to_string()));
        assert!(
            build_vision_decoder_ffmpeg_args("file:///etc/shadow", 640, 360, 5).is_none(),
            "non-RTSP URLs must be rejected"
        );

        // Odd and extreme dimensions are clamped and rounded down to even YUV420p plane sizes
        let (w_odd, h_odd, len_odd) = sanitize_yuv420p_dimensions(161, 121);
        assert_eq!((w_odd, h_odd), (160, 120));
        assert_eq!(len_odd, (160 * 120 * 3) / 2);

        let (w_huge, h_huge, len_huge) = sanitize_yuv420p_dimensions(u32::MAX, u32::MAX);
        assert_eq!((w_huge, h_huge), (MAX_DETECT_WIDTH, MAX_DETECT_HEIGHT));
        assert_eq!(
            len_huge,
            ((MAX_DETECT_WIDTH as usize) * (MAX_DETECT_HEIGHT as usize) * 3) / 2
        );
    }

    #[tokio::test]
    async fn lifecycle_redacts_event_base64_thumbnail_scales_pixel_privacy_mask_and_handles_odd_frames()
     {
        let (pipeline, tmp_dir) = build_isolated_pipeline("privacy-thumb-odd");
        let camera = "front_door";

        let mut cam_cfg = CameraConfig::default();
        cam_cfg.snapshots.enabled = true;
        cam_cfg.snapshots.high_res = false;
        cam_cfg.snapshots.from_main_stream = false;
        cam_cfg.review.alerts.enabled = true;
        cam_cfg.review.alerts.labels = vec!["person".to_string()];
        // Pixel-space privacy mask covering top-left 128x72 on a 320x181 (odd height) frame
        cam_cfg.privacy_mask = json!(["0,0,128,0,128,72,0,72"]);

        let w = 320u32;
        let h = 181u32; // odd height that previously triggered out-of-bounds panic in chroma plane
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let mut frame_bytes = vec![235u8; y_len + uv_len * 2];
        frame_bytes[y_len..].fill(128);
        let odd_yuv_frame = YuvFrame {
            camera: Arc::from(camera),
            timestamp: 5000.0,
            width: w,
            height: h,
            data: Arc::new(frame_bytes),
        };

        let mut active_review: Option<ActiveReviewState> = None;
        let mut track_snapshots: HashMap<String, TrackPersistSnapshot> = HashMap::new();

        let mut trk = sample_track();
        trk.id = "1730505000.000000-priv01".to_string();
        trk.bbox = BoundingBox::new(10, 10, 100, 100);
        trk.region = BoundingBox::new(0, 0, 160, 160);

        let ctx = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &odd_yuv_frame,
            now: 5000.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx,
                std::slice::from_ref(&trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;

        // 1. Event base64 thumbnail (`ev["thumbnail"]`) must have privacy mask redacted
        let ev = pipeline.db.get_event(&trk.id).unwrap().unwrap();
        let thumb_b64 = ev["thumbnail"]
            .as_str()
            .expect("event thumbnail must be stored");
        let thumb_bytes = base64::engine::general_purpose::STANDARD
            .decode(thumb_b64)
            .unwrap();
        let thumb_img = image::load_from_memory(&thumb_bytes).unwrap().to_rgb8();
        assert_eq!(
            *thumb_img.get_pixel(10, 10),
            image::Rgb([12, 14, 18]),
            "cropped event thumbnail must redact pixels inside privacy_mask"
        );

        // 2. Review thumbnail scaled to 180p must also redact pixel-coordinate privacy mask
        let rev_id = active_review.as_ref().unwrap().id.clone();
        let rev_thumb_path = pipeline
            .media_dir
            .join("clips")
            .join("review")
            .join(format!("thumb-{camera}-{rev_id}.webp"));
        let rev_img = image::open(&rev_thumb_path).unwrap().to_rgb8();
        assert_eq!(
            *rev_img.get_pixel(10, 10),
            image::Rgb([12, 14, 18]),
            "downscaled review thumbnail must redact normalized pixel privacy_mask"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn mid_track_false_positive_suppression_downgrades_active_track_and_closes_review() {
        let (pipeline, tmp_dir) = build_isolated_pipeline("mid-track-fp");
        let camera = "driveway";
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.review.alerts.enabled = true;
        cam_cfg.review.alerts.labels = vec!["person".to_string()];

        let w = 320u32;
        let h = 180u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let yuv_frame = YuvFrame {
            camera: Arc::from(camera),
            timestamp: 6000.0,
            width: w,
            height: h,
            data: Arc::new(vec![128u8; y_len + uv_len * 2]),
        };

        let mut active_review: Option<ActiveReviewState> = None;
        let mut track_snapshots: HashMap<String, TrackPersistSnapshot> = HashMap::new();
        let mut trk = sample_track();
        trk.id = "1730506000.000000-midfp".to_string();
        trk.score = 0.80;

        // Frame 1: track starts as a normal alert and opens active_review
        let ctx1 = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 6000.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx1,
                std::slice::from_ref(&trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        assert!(active_review.is_some());

        // Operator / GenAI marks active event as false_positive = true mid-track
        pipeline.db.set_event_false_positive(&trk.id, true).unwrap();

        // Frame 2: score increases on same active track; pipeline must detect DB false_positive,
        // downgrade max_severity to "detection", and clear the single-track active_review
        trk.score = 0.92;
        trk.top_score = 0.92;
        let ctx2 = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 6002.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx2,
                std::slice::from_ref(&trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        assert!(
            active_review.is_none(),
            "active_review must be cleared when its only detection is marked false_positive"
        );
        assert!(
            pipeline
                .db
                .query_reviews(&ReviewFilter::default())
                .unwrap()
                .is_empty(),
            "single-track reviewsegment must also be removed from SQLite when its only track is marked false_positive"
        );
        let ev_after = pipeline.db.get_event(&trk.id).unwrap().unwrap();
        assert_eq!(ev_after["false_positive"], json!(true));
        assert_eq!(ev_after["data"]["max_severity"], json!("detection"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn cycle2_pipeline_symlink_containment_stationary_review_dedup_fp_db_cleanup_and_url_bounds()
     {
        let (pipeline, tmp_dir) = build_isolated_pipeline("cycle2-hardening");
        let camera = "driveway";
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.snapshots.enabled = true;
        cam_cfg.review.alerts.enabled = true;
        cam_cfg.review.alerts.labels = vec!["car".to_string(), "person".to_string()];
        cam_cfg.review.alerts.cutoff_time = 20;

        // 1. PIPE-C2-04: RTSP URL validation rejects hostless query/fragment, empty IPv6 brackets, and invalid ports
        for invalid_rtsp in [
            "rtsp://?transport=tcp",
            "rtsp://#frag",
            "rtsp://user:pass@?foo=1",
            "rtsp://[]:554/sub",
            "rtsp://[::1:554/sub",
            "rtsp://192.0.2.10:0/sub",
            "rtsp://192.0.2.10:abc/sub",
            "rtsp://192.0.2.10:99999/sub",
            "rtsp://192.0.2.10\\evil/sub",
        ] {
            assert!(
                !is_valid_rtsp_stream_url(invalid_rtsp),
                "invalid RTSP URL '{invalid_rtsp}' must be rejected"
            );
        }
        assert!(is_valid_rtsp_stream_url(
            "rtsp://user:pass@[2001:db8::1]:8554/sub?channel=1"
        ));

        // Privacy mask coordinate normalization rejects malformed/NaN tokens without shifting X/Y parity
        let bad_mask = json!(["0,0,bad_token,100,0,100,100,0,100", "0,0,NaN,0,100,100"]);
        let norm_bad = normalize_privacy_mask_for_resolution(&bad_mask, 320, 180);
        assert_eq!(norm_bad, json!(["", ""]));
        // 0x0 RgbImage does not panic in crop_and_scale_rgb_image
        let zero_rgb = RgbImage::new(0, 0);
        let safe_crop =
            crop_and_scale_rgb_image(&zero_rgb, BoundingBox::new(0, 0, 10, 10), Some(64));
        assert_eq!((safe_crop.width(), safe_crop.height()), (1, 1));

        // 2. PIPE-C2-01: write_contained_media_file refuses symlinked target files and symlinked subdirectories,
        //    and decode_bounded_high_res_snapshot rejects images exceeding MAX_DETECT_WIDTH/HEIGHT
        let clips_dir = pipeline.media_dir.join("clips");
        std::fs::create_dir_all(&clips_dir).unwrap();
        let outside_secret = tmp_dir.join("outside_secret.txt");
        std::fs::write(&outside_secret, b"DO_NOT_OVERWRITE").unwrap();
        #[cfg(unix)]
        {
            let symlink_snap = clips_dir.join("symlink_attack.jpg");
            std::os::unix::fs::symlink(&outside_secret, &symlink_snap).unwrap();
            assert!(
                write_contained_media_file(
                    &pipeline.media_dir,
                    &clips_dir,
                    "symlink_attack.jpg",
                    b"evil"
                )
                .is_err(),
                "writing through a symlinked snapshot file must fail"
            );
            assert_eq!(
                std::fs::read(&outside_secret).unwrap(),
                b"DO_NOT_OVERWRITE",
                "symlink target outside media_dir must remain untouched"
            );
        }
        assert!(
            write_contained_media_file(&pipeline.media_dir, &clips_dir, "../escape.jpg", b"x")
                .is_err()
        );
        assert!(
            write_contained_media_file(&pipeline.media_dir, &clips_dir, ".hidden.jpg", b"x")
                .is_err()
        );

        let valid_hr_rgb = RgbImage::from_pixel(640, 360, image::Rgb([80, 90, 100]));
        let valid_hr_jpg = encode_image_jpeg(&valid_hr_rgb).unwrap();
        assert!(decode_bounded_high_res_snapshot(&valid_hr_jpg, 320, 180).is_some());
        // Undersized high-res frame (< det_w x det_h) is rejected before pixel decode
        assert!(decode_bounded_high_res_snapshot(&valid_hr_jpg, 1280, 720).is_none());

        // 3. PIPE-C2-03: Stationary car whose initial review segment expires must NOT re-open endless
        //    duplicate alert ReviewSegments on subsequent frames while remaining stationary
        let w = 320u32;
        let h = 180u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let yuv_frame = YuvFrame {
            camera: Arc::from(camera),
            timestamp: 7000.0,
            width: w,
            height: h,
            data: Arc::new(vec![128u8; y_len + uv_len * 2]),
        };

        let mut active_review: Option<ActiveReviewState> = None;
        let mut track_snapshots: HashMap<String, TrackPersistSnapshot> = HashMap::new();
        let mut car_trk = sample_track();
        car_trk.id = "1730507000.000000-parkedcar".to_string();
        car_trk.label = "car".to_string();
        car_trk.stationary = false;

        // t=7000: car arrives (moving) -> opens ReviewSegment #1
        let ctx_arrive = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 7000.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx_arrive,
                std::slice::from_ref(&car_trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        assert!(active_review.is_some());

        // t=7005: car becomes stationary -> review stays open until cutoff_time (20s)
        car_trk.stationary = true;
        let ctx_park = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 7005.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx_park,
                std::slice::from_ref(&car_trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        assert!(active_review.is_some());

        // t=7030 (> 7000 + 20s cutoff): ReviewSegment #1 expires and closes
        let ctx_expire = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 7030.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx_expire,
                std::slice::from_ref(&car_trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        assert!(
            active_review.is_none(),
            "stationary car review must expire after cutoff_time"
        );

        // t=7031: stationary car is STILL in active_tracks -> must NOT open a duplicate ReviewSegment #2!
        let ctx_next_frame = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 7031.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx_next_frame,
                std::slice::from_ref(&car_trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        assert!(
            active_review.is_none(),
            "already-stationary track must not re-open a new ReviewSegment after expiration"
        );
        let all_reviews = pipeline.db.query_reviews(&ReviewFilter::default()).unwrap();
        assert_eq!(all_reviews.len(), 1);
        assert!(all_reviews[0].end_time.is_some());

        // 4. PIPE-C2-02: Overflowing track_snapshots with stale tracks preserves active track state
        for idx in 0..(MAX_TRACK_SNAPSHOTS + 16) {
            track_snapshots.insert(
                format!("stale-trk-{idx}"),
                TrackPersistSnapshot {
                    best_score: 0.5,
                    ai_verify_triggered: true,
                    ..Default::default()
                },
            );
        }
        track_snapshots
            .get_mut(&car_trk.id)
            .unwrap()
            .ai_verify_triggered = true;
        let ctx_overflow = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 7032.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx_overflow,
                std::slice::from_ref(&car_trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        assert!(
            track_snapshots.len() <= MAX_TRACK_SNAPSHOTS,
            "track_snapshots must be bounded by MAX_TRACK_SNAPSHOTS"
        );
        assert!(
            track_snapshots
                .get(&car_trk.id)
                .is_some_and(|s| s.ai_verify_triggered),
            "active track snapshot state (ai_verify_triggered) must be preserved across eviction"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn cycle3_pipeline_multitrack_fp_severity_preservation_utf8_rtsp_bounds_and_media_prefix_guard()
     {
        let (pipeline, tmp_dir) = build_isolated_pipeline("cycle3-hardening");
        let camera = "front_yard";
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.snapshots.enabled = true;
        cam_cfg.review.alerts.enabled = true;
        cam_cfg.review.alerts.labels = vec!["person".to_string()];
        cam_cfg.review.detections.enabled = true;

        // 1. PIPE-C3-01: Multi-byte UTF-8 characters spanning byte indices 7 or 8 must not panic in is_valid_rtsp_stream_url
        for non_ascii_or_malformed in [
            "123456\u{00e9}89",
            "12345\u{00e9}789",
            "\u{1f525}rtsp://192.0.2.1/sub",
            "rtsp://@192.0.2.1/sub",
            "rtsp://bad.-label.local/sub",
            "rtsp://label-.bad.local/sub",
        ] {
            assert!(
                !is_valid_rtsp_stream_url(non_ascii_or_malformed),
                "malformed/UTF-8 URL '{non_ascii_or_malformed}' must be rejected without panicking"
            );
        }

        // 2. PIPE-C3-02: write_contained_media_file rejects target_dir outside media_dir before creating directories
        let outside_dir = tmp_dir.join("outside_uncreated_subdir");
        assert!(
            write_contained_media_file(&pipeline.media_dir, &outside_dir, "snap.jpg", b"test")
                .is_err(),
            "target_dir outside media_dir must be rejected"
        );
        assert!(
            !outside_dir.exists(),
            "write_contained_media_file must not create directories outside media_dir"
        );

        // 3. PIPE-C3-03: Suppressing one false-positive track in a multi-track review segment must NOT
        //    downgrade the review segment to "detection" while a legitimate "alert" track remains in rev.detections,
        //    whereas suppressing the sole "alert" track when only a "detection" ("cat") remains downgrades to "detection"
        let w = 320u32;
        let h = 180u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let yuv_frame = YuvFrame {
            camera: Arc::from(camera),
            timestamp: 8000.0,
            width: w,
            height: h,
            data: Arc::new(vec![128u8; y_len + uv_len * 2]),
        };

        let mut active_review: Option<ActiveReviewState> = None;
        let mut track_snapshots: HashMap<String, TrackPersistSnapshot> = HashMap::new();

        let mut trk_real_person = sample_track();
        trk_real_person.id = "1730508000.000000-realperson".to_string();
        trk_real_person.label = "person".to_string();
        trk_real_person.score = 0.90;

        let mut trk_fp_person = sample_track();
        trk_fp_person.id = "1730508000.000000-fpperson".to_string();
        trk_fp_person.label = "person".to_string();
        trk_fp_person.score = 0.81;

        // Frame 1: both person tracks are active -> active_review has severity "alert" and 2 detections
        let ctx1 = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 8000.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx1,
                &[trk_real_person.clone(), trk_fp_person.clone()],
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        assert_eq!(
            active_review.as_ref().map(|r| r.severity.as_str()),
            Some("alert")
        );
        assert_eq!(active_review.as_ref().unwrap().detections.len(), 2);

        // Operator / GenAI marks trk_fp_person as false_positive = true while trk_real_person remains valid
        pipeline
            .db
            .set_event_false_positive(&trk_fp_person.id, true)
            .unwrap();
        trk_fp_person.score = 0.88;
        let ctx2 = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 8002.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx2,
                &[trk_real_person.clone(), trk_fp_person.clone()],
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        let rev_after_fp = active_review
            .as_ref()
            .expect("review must remain open for trk_real_person");
        assert_eq!(rev_after_fp.detections, vec![trk_real_person.id.clone()]);
        assert_eq!(
            rev_after_fp.severity, "alert",
            "suppressing one FP track must preserve 'alert' severity when another alert track remains"
        );

        // Now add a non-alert detection track ("cat") and mark trk_real_person as false_positive on track end
        let mut trk_cat = sample_track();
        trk_cat.id = "1730508003.000000-catdet".to_string();
        trk_cat.label = "cat".to_string();
        trk_cat.score = 0.84;
        let ctx3 = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 8003.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx3,
                &[trk_real_person.clone(), trk_cat.clone()],
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        assert_eq!(active_review.as_ref().unwrap().detections.len(), 2);

        pipeline
            .db
            .set_event_false_positive(&trk_real_person.id, true)
            .unwrap();
        let ctx4 = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 8004.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx4,
                std::slice::from_ref(&trk_cat),
                std::slice::from_ref(&trk_real_person),
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        let rev_cat_only = active_review
            .as_ref()
            .expect("review must remain open for cat detection");
        assert_eq!(rev_cat_only.detections, vec![trk_cat.id.clone()]);
        assert_eq!(
            rev_cat_only.severity, "detection",
            "when the last alert track ends as FP and only a detection track remains, review severity must downgrade to 'detection'"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn titlecase_and_mixed_case_profiles_suppress_recording_and_alerts_and_motion_px_saturates()
     {
        let (pipeline, tmp_dir) = build_isolated_pipeline("strcmp-arith-p2a");
        let camera = "front_patio";

        // 1. STRCMP-004: TitleCase ('Home') and mixed-case ('NightWatch') profile keys in cfg.profiles
        //    must match normalized lowercase active_profile ('home', 'nightwatch') in both
        //    lookup_profile_override / is_camera_alert_suppressed_in_profile and process_lifecycle_events
        {
            let mut cfg = pipeline.config.write().await;
            cfg.profiles.insert(
                "Home".to_string(),
                ProfileOverrideConfig {
                    disabled_alerts_cameras: vec![camera.to_string()],
                    disabled_record_cameras: vec![camera.to_string()],
                },
            );
            cfg.profiles.insert(
                "NightWatch".to_string(),
                ProfileOverrideConfig {
                    disabled_alerts_cameras: vec!["*".to_string()],
                    disabled_record_cameras: vec!["*".to_string()],
                },
            );
            assert!(lookup_profile_override(&cfg, "home").is_some());
            assert!(lookup_profile_override(&cfg, "nightwatch").is_some());
            assert!(is_camera_alert_suppressed_in_profile(&cfg, "home", camera));
            assert!(is_camera_alert_suppressed_in_profile(
                &cfg,
                "nightwatch",
                camera
            ));
        }

        let mut cam_cfg = CameraConfig::default();
        cam_cfg.record.enabled = true;
        cam_cfg.snapshots.enabled = false;
        cam_cfg.review.alerts.enabled = true;
        cam_cfg.review.alerts.labels = vec!["person".to_string()];

        let w = 320u32;
        let h = 180u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let yuv_frame = YuvFrame {
            camera: Arc::from(camera),
            timestamp: 9000.0,
            width: w,
            height: h,
            data: Arc::new(vec![128u8; y_len + uv_len * 2]),
        };

        // Activate "Home" via CommsHub (which normalizes active_profile to "home")
        assert_eq!(pipeline.comms.set_active_profile("Home"), "home");
        let mut active_review: Option<ActiveReviewState> = None;
        let mut track_snapshots: HashMap<String, TrackPersistSnapshot> = HashMap::new();
        let mut trk_home = sample_track();
        trk_home.id = "1730509000.000000-homeprof".to_string();

        let ctx_home = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 9000.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx_home,
                std::slice::from_ref(&trk_home),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        let ev_home = pipeline.db.get_event(&trk_home.id).unwrap().unwrap();
        assert_eq!(
            ev_home["has_clip"],
            json!(false),
            "TitleCase 'Home' profile must suppress recording (has_clip=false) when active_profile is 'home'"
        );
        assert_eq!(
            ev_home["data"]["max_severity"],
            json!("detection"),
            "TitleCase 'Home' profile must downgrade alert to detection when active_profile is 'home'"
        );

        // Activate "NightWatch" via CommsHub (normalizes to "nightwatch")
        assert_eq!(
            pipeline.comms.set_active_profile("NightWatch"),
            "nightwatch"
        );
        let mut trk_nw = sample_track();
        trk_nw.id = "1730509010.000000-nwprof".to_string();
        let ctx_nw = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 9010.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx_nw,
                std::slice::from_ref(&trk_nw),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;
        let ev_nw = pipeline.db.get_event(&trk_nw.id).unwrap().unwrap();
        assert_eq!(
            ev_nw["has_clip"],
            json!(false),
            "Mixed-case 'NightWatch' profile must suppress recording when active_profile is 'nightwatch'"
        );
        assert_eq!(
            ev_nw["data"]["max_severity"],
            json!("detection"),
            "Mixed-case 'NightWatch' profile must suppress alerts when active_profile is 'nightwatch'"
        );

        // 2. ARITHOFL-002: Overlapping motion bounding boxes whose areas sum > u32::MAX must saturate
        //    without panicking or wrapping and clamp to width.saturating_mul(height)
        let frame_w = 3840u32;
        let frame_h = 2160u32;
        let full_box = BoundingBox::new(0, 0, frame_w, frame_h);
        // 600 overlapping full-frame 4K boxes: 600 * 8,294,400 = 4,976,640,000 > u32::MAX (4,294,967,295)
        let overlapping_boxes: Vec<BoundingBox> = vec![full_box; 600];
        let raw_saturated = overlapping_boxes
            .iter()
            .fold(0u32, |acc, b| acc.saturating_add(b.area()));
        assert_eq!(raw_saturated, u32::MAX);
        let motion_px: u32 = overlapping_boxes
            .iter()
            .fold(0u32, |acc, b| acc.saturating_add(b.area()))
            .min(frame_w.saturating_mul(frame_h));
        assert_eq!(
            motion_px,
            frame_w * frame_h,
            "saturating motion_px accumulation must clamp to frame area without wrapping"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn watchdog_camera_reboot_triggers_onvif_system_reboot_when_rtsp_stalls() {
        use axum::{Router, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let reboot_hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = reboot_hits.clone();
        let mock_router = Router::new().route(
            "/onvif/device_service",
            post(move |body: String| {
                let h = hits_srv.clone();
                async move {
                    if body.contains("SystemReboot") && body.contains("PasswordDigest") {
                        h.fetch_add(1, Ordering::SeqCst);
                        (
                            axum::http::StatusCode::OK,
                            r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:SystemRebootResponse><tds:Message>Rebooting</tds:Message></tds:SystemRebootResponse></s:Body></s:Envelope>"#,
                        )
                    } else {
                        (axum::http::StatusCode::BAD_REQUEST, "")
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, mock_router).await;
        });

        let yaml = format!(
            r#"
cameras:
  quarto:
    enabled: true
    onvif:
      host: 127.0.0.1
      port: {}
      user: admin
      password: secret
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/quarto_sub
          roles: [detect]
"#,
            addr.port()
        );
        let cfg =
            NinjaConfig::from_yaml_str(&yaml, PathBuf::from("/tmp/test_watchdog.yml")).unwrap();
        let rt = crate::camera_protocols::CameraDriverRuntime::new();
        assert!(try_watchdog_camera_reboot(&cfg, "quarto", &rt).await);
        assert_eq!(reboot_hits.load(Ordering::SeqCst), 1);
        srv.abort();
    }

    #[tokio::test]
    async fn zone_entry_and_line_crossing_with_siren_enabled_triggers_audible_camera_alarm() {
        let (pipeline, tmp_dir) = build_isolated_pipeline("zone-line-siren");
        let camera = "escritorio";

        let mut cam_cfg = CameraConfig::default();
        cam_cfg.onvif.host = Some("127.0.0.1".to_string());
        cam_cfg.onvif.port = 2020;
        cam_cfg.hardware.protocol = Some("tapo".to_string());
        cam_cfg.hardware.cloud_password = Some("test_cloud_pw".to_string());
        cam_cfg.review.alerts.enabled = true;
        cam_cfg.review.alerts.labels = vec!["person".to_string()];
        cam_cfg.lines.insert(
            "cantinho".to_string(),
            ninjanvr_config::LineConfig {
                points: [0.2, 0.2, 0.8, 0.2],
                direction: ninjanvr_config::LineDirection::AToB,
                objects: vec!["person".to_string()],
                siren: true,
            },
        );

        let mut ws_rx = pipeline.comms.subscribe_ws();
        let w = 320u32;
        let h = 180u32;
        let y_len = (w * h) as usize;
        let uv_len = y_len / 4;
        let yuv_frame = YuvFrame {
            camera: Arc::from(camera),
            timestamp: 9500.0,
            width: w,
            height: h,
            data: Arc::new(vec![128u8; y_len + uv_len * 2]),
        };

        let mut active_review: Option<ActiveReviewState> = None;
        let mut track_snapshots: HashMap<String, TrackPersistSnapshot> = HashMap::new();
        let mut trk = sample_track();
        trk.id = "1730509500.000000-sirenline".to_string();
        trk.label = "person".to_string();
        trk.crossed_lines = vec!["cantinho".to_string()];
        trk.entered_zones = vec!["cantinho".to_string()];

        let ctx = FrameLifecycleCtx {
            camera,
            cam_cfg: &cam_cfg,
            frame: &yuv_frame,
            now: 9500.0,
        };
        pipeline
            .process_lifecycle_events(
                &ctx,
                std::slice::from_ref(&trk),
                &[],
                &mut active_review,
                &mut track_snapshots,
            )
            .await;

        let ws_msgs = drain_ws_messages(&mut ws_rx);
        assert!(
            ws_msgs.iter().any(|m| {
                m["topic"] == "siren"
                    && serde_json::from_str::<Value>(m["payload"].as_str().unwrap_or("{}"))
                        .is_ok_and(|p| {
                            p["camera"] == "escritorio" && p["trigger"] == "line:cantinho"
                        })
            }),
            "crossing a line with siren=true must publish a 'siren' event"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
