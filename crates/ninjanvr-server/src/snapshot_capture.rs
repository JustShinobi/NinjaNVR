//! Unified operator snapshot capture (`GET /api/{camera}/snapshot.jpg`).
//!
//! The console used to offer two different "save a picture" paths for the same live
//! camera: a browser-side PNG grabbed from whatever `<video>` stream happened to be
//! playing (sub or main, never privacy-masked) and a detect-resolution JPEG from
//! `latest.jpg`. This module backs a single server-side snapshot that always tries
//! the full-resolution main stream first, enforces the camera privacy mask
//! (fail-closed), and only then falls back to the latest detect frame.

use chrono::NaiveDateTime;
use serde_json::Value;

use crate::pipeline::sanitize_filename_component;

/// JPEG quality used when a snapshot must be re-encoded (privacy mask applied).
pub const SNAPSHOT_JPEG_QUALITY: u8 = 92;

/// Upper bound for how long the main-stream frame grab may block the request.
pub const MAIN_STREAM_FRAME_TIMEOUT_MS: u64 = 2500;

/// Which pipeline produced the bytes returned to the operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotSource {
    /// Full-resolution frame grabbed from the camera main stream via go2rtc.
    MainStream,
    /// Latest decoded detect-pipeline frame (substream resolution).
    DetectFrame,
}

impl SnapshotSource {
    pub fn as_header_value(self) -> &'static str {
        match self {
            SnapshotSource::MainStream => "main",
            SnapshotSource::DetectFrame => "detect",
        }
    }
}

/// Builds the download filename `{camera}_{YYYYMMDD-HHMMSS}.jpg`, sanitizing the camera id
/// so the `Content-Disposition` header can never carry quotes, separators or traversal.
pub fn snapshot_download_filename(camera: &str, captured_at: NaiveDateTime) -> String {
    format!(
        "{}_{}.jpg",
        sanitize_filename_component(camera),
        captured_at.format("%Y%m%d-%H%M%S")
    )
}

/// `Content-Disposition` value that makes browsers (desktop and mobile) save the file.
pub fn snapshot_content_disposition(filename: &str) -> String {
    format!("attachment; filename=\"{filename}\"")
}

/// Renders a main-stream frame into the bytes served to the operator.
///
/// * Without an active privacy mask the camera JPEG is passed through untouched (no
///   generational loss, no CPU spent re-encoding) once its header dimensions validate.
/// * With an active mask the frame is decoded under bounded allocation limits, masked,
///   and re-encoded. If masking fails, `None` is returned so the caller falls back to a
///   path that is already masked — an unmasked frame is never served.
///
/// Returns `(jpeg_bytes, width, height)`.
pub fn render_main_stream_snapshot(
    bytes: &[u8],
    privacy_mask: Option<&Value>,
    detect_w: u32,
    detect_h: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    let mask = privacy_mask.filter(|m| crate::has_active_privacy_mask(Some(m)));
    match mask {
        None => {
            let (w, h) = probe_jpeg_dimensions(bytes)?;
            Some((bytes.to_vec(), w, h))
        }
        Some(mask) => {
            let mut rgb = crate::decode_bounded_snapshot_rgb(bytes)?;
            if !crate::apply_camera_privacy_mask_or_fail_closed(&mut rgb, mask, detect_w, detect_h)
            {
                return None;
            }
            let (w, h) = (rgb.width(), rgb.height());
            let encoded =
                ninjanvr_vision::encode_image_jpeg_with_quality(&rgb, SNAPSHOT_JPEG_QUALITY)
                    .ok()?;
            Some((encoded, w, h))
        }
    }
}

fn probe_jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.is_empty() || bytes.len() > crate::MAX_SNAPSHOT_FRAME_BYTES {
        return None;
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    if reader.format() != Some(image::ImageFormat::Jpeg) {
        return None;
    }
    let (w, h) = reader.into_dimensions().ok()?;
    if w == 0 || h == 0 || w > crate::MAX_SNAPSHOT_DIMENSION || h > crate::MAX_SNAPSHOT_DIMENSION {
        return None;
    }
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use serde_json::json;

    fn white_jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut img = image::RgbImage::new(w, h);
        for p in img.pixels_mut() {
            *p = image::Rgb([250, 250, 250]);
        }
        ninjanvr_vision::encode_image_jpeg(&img).unwrap()
    }

    fn ts() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 7)
            .unwrap()
            .and_hms_opt(22, 56, 54)
            .unwrap()
    }

    #[test]
    fn filename_uses_camera_and_local_timestamp() {
        assert_eq!(
            snapshot_download_filename("escritorio", ts()),
            "escritorio_20261107-225654.jpg"
        );
    }

    #[test]
    fn filename_sanitizes_header_breaking_characters() {
        let name = snapshot_download_filename("a\"b/..\\c", ts());
        assert!(!name.contains('"'));
        assert!(!name.contains('/'));
        assert!(!name.contains('\\'));
        assert!(!name.contains(".."));
        assert!(name.ends_with("_20261107-225654.jpg"));
        assert_eq!(
            snapshot_content_disposition("x.jpg"),
            "attachment; filename=\"x.jpg\""
        );
    }

    #[test]
    fn unmasked_main_stream_frame_is_passed_through_at_native_resolution() {
        let jpg = white_jpeg(1920, 1080);
        let (out, w, h) = render_main_stream_snapshot(&jpg, None, 1280, 720).unwrap();
        assert_eq!((w, h), (1920, 1080));
        assert_eq!(out, jpg, "no mask must not re-encode the camera JPEG");
    }

    #[test]
    fn empty_mask_values_count_as_unmasked() {
        let jpg = white_jpeg(64, 36);
        let empty = json!([]);
        let (out, _, _) = render_main_stream_snapshot(&jpg, Some(&empty), 64, 36).unwrap();
        assert_eq!(out, jpg);
    }

    #[test]
    fn active_privacy_mask_is_burned_into_full_resolution_frame() {
        let jpg = white_jpeg(320, 180);
        let mask = json!(["0.0,0.0,1.0,0.0,1.0,1.0,0.0,1.0"]);
        let (out, w, h) = render_main_stream_snapshot(&jpg, Some(&mask), 160, 90).unwrap();
        assert_eq!((w, h), (320, 180));
        let img = image::load_from_memory(&out).unwrap().to_rgb8();
        assert!(
            img.get_pixel(160, 90).0[0] < 30,
            "masked area must be blacked out"
        );
    }

    #[test]
    fn invalid_or_non_jpeg_payloads_are_rejected() {
        assert!(render_main_stream_snapshot(b"", None, 0, 0).is_none());
        assert!(render_main_stream_snapshot(b"<html>go2rtc error</html>", None, 0, 0).is_none());
        let mut png = Vec::new();
        image::RgbImage::new(8, 8)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert!(render_main_stream_snapshot(&png, None, 0, 0).is_none());
    }

    #[test]
    fn source_header_values_are_stable() {
        assert_eq!(SnapshotSource::MainStream.as_header_value(), "main");
        assert_eq!(SnapshotSource::DetectFrame.as_header_value(), "detect");
    }
}
