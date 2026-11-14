//! Centralized server-side privacy-mask policy resolution, fail-closed RGB/image-byte
//! redaction, event-slice base64 thumbnail redaction, and untranscoded video gating.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use image::{Rgb, RgbImage};
use ninjanvr_config::NinjaConfig;
use ninjanvr_vision::PrivacyMaskPolicy;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::{MAX_SNAPSHOT_DECODE_ALLOC_BYTES, MAX_SNAPSHOT_DIMENSION, MAX_SNAPSHOT_FRAME_BYTES};

/// Decodes an encoded snapshot (`JPEG`/`WebP`/`PNG`) into an [`RgbImage`] with strict byte,
/// dimension, and heap-allocation limits.
pub fn decode_bounded_snapshot_rgb(bytes: &[u8]) -> Option<RgbImage> {
    if bytes.is_empty() || bytes.len() > MAX_SNAPSHOT_FRAME_BYTES {
        return None;
    }
    let dim_reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let (w, h) = dim_reader.into_dimensions().ok()?;
    if w == 0 || h == 0 || w > MAX_SNAPSHOT_DIMENSION || h > MAX_SNAPSHOT_DIMENSION {
        return None;
    }
    let rgba_bytes = (w as u64).saturating_mul(h as u64).saturating_mul(4);
    if rgba_bytes > MAX_SNAPSHOT_DECODE_ALLOC_BYTES {
        return None;
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SNAPSHOT_DIMENSION);
    limits.max_image_height = Some(MAX_SNAPSHOT_DIMENSION);
    limits.max_alloc = Some(MAX_SNAPSHOT_DECODE_ALLOC_BYTES);
    reader.limits(limits);
    Some(reader.decode().ok()?.to_rgb8())
}

fn encode_dark_placeholder(prefer_jpeg: bool) -> (Vec<u8>, &'static str) {
    let fallback_rgb = RgbImage::from_pixel(64, 36, Rgb([12, 14, 18]));
    if prefer_jpeg {
        (
            ninjanvr_vision::encode_image_jpeg(&fallback_rgb).unwrap_or_default(),
            "image/jpeg",
        )
    } else {
        (
            ninjanvr_vision::encode_image_webp(&fallback_rgb)
                .unwrap_or_else(|_| b"RIFF\x1a\x00\x00\x00WEBPVP8 ".to_vec()),
            "image/webp",
        )
    }
}

fn extract_event_crop_xywh(ev: &Value) -> Option<[f64; 4]> {
    let parse4 = |v: &Value| -> Option<[f64; 4]> {
        let arr = v.as_array()?;
        if arr.len() < 4 {
            return None;
        }
        let coords = [
            arr[0].as_f64()?,
            arr[1].as_f64()?,
            arr[2].as_f64()?,
            arr[3].as_f64()?,
        ];
        if !coords.iter().all(|x| x.is_finite()) || coords.iter().all(|&x| x <= 0.0) {
            return None;
        }
        Some(coords)
    };

    if let Some([rx, ry, rw, rh]) = parse4(&ev["data"]["region"])
        && (0.0..1.0).contains(&rx)
        && (0.0..1.0).contains(&ry)
        && rw >= 0.005
        && rh >= 0.005
        && rx + rw <= 1.001
        && ry + rh <= 1.001
    {
        return Some([rx, ry, rw.min(1.0 - rx), rh.min(1.0 - ry)]);
    }

    if let Some([a, b, c, d]) = parse4(&ev["data"]["box"]).or_else(|| parse4(&ev["box"]))
        && (0.0..1.0).contains(&a)
        && (0.0..1.0).contains(&b)
    {
        if c >= 0.005 && d >= 0.005 && a + c <= 1.001 && b + d <= 1.001 {
            return Some([a, b, c.min(1.0 - a), d.min(1.0 - b)]);
        }
        if c <= 1.001 && d <= 1.001 && c > a + 0.005 && d > b + 0.005 {
            return Some([a, b, (c - a).min(1.0 - a), (d - b).min(1.0 - b)]);
        }
    }
    None
}

/// `AppState`-owned gate enforcing camera [`PrivacyMaskPolicy`] across snapshots, thumbnails,
/// dataset exports, static media, share links, and untranscoded MP4 / HLS VOD endpoints.
#[derive(Clone)]
pub struct MediaDeliveryGate {
    config: Arc<RwLock<NinjaConfig>>,
}

impl MediaDeliveryGate {
    pub fn new(config: Arc<RwLock<NinjaConfig>>) -> Self {
        Self { config }
    }

    /// Resolves the canonical [`PrivacyMaskPolicy`] for `camera` from an already-locked `NinjaConfig`.
    pub fn camera_mask_policy_from_config(cfg: &NinjaConfig, camera: &str) -> PrivacyMaskPolicy {
        let Some(cam_cfg) = cfg.cameras.get(camera) else {
            return PrivacyMaskPolicy::Disabled;
        };
        PrivacyMaskPolicy::from_value(
            &cam_cfg.privacy_mask,
            cam_cfg.detect.width.max(1),
            cam_cfg.detect.height.max(1),
        )
    }

    /// Collects all cameras whose privacy mask is configured (`Active` or `InvalidFailClosed`).
    pub fn configured_camera_policies(cfg: &NinjaConfig) -> BTreeMap<String, PrivacyMaskPolicy> {
        cfg.cameras
            .iter()
            .filter_map(|(name, cam_cfg)| {
                let policy = PrivacyMaskPolicy::from_value(
                    &cam_cfg.privacy_mask,
                    cam_cfg.detect.width.max(1),
                    cam_cfg.detect.height.max(1),
                );
                policy.is_configured().then(|| (name.clone(), policy))
            })
            .collect()
    }

    /// Resolves the canonical [`PrivacyMaskPolicy`] for `camera`.
    pub async fn camera_mask_policy(&self, camera: &str) -> PrivacyMaskPolicy {
        let cfg = self.config.read().await;
        Self::camera_mask_policy_from_config(&cfg, camera)
    }

    /// Returns the normalized JSON mask value for `camera` (`None` if the camera is unknown).
    pub async fn camera_normalized_mask_value(&self, camera: &str) -> Option<Value> {
        let cfg = self.config.read().await;
        cfg.cameras.get(camera).map(|c| {
            let policy = PrivacyMaskPolicy::from_value(
                &c.privacy_mask,
                c.detect.width.max(1),
                c.detect.height.max(1),
            );
            if policy.is_configured() {
                policy.to_normalized_value()
            } else {
                c.privacy_mask.clone()
            }
        })
    }

    /// Returns `true` when raw untranscoded video (MP4 / HLS VOD) must be blocked (`404`) for `camera`.
    pub fn blocks_untranscoded_video_in_config(cfg: &NinjaConfig, camera: &str) -> bool {
        Self::camera_mask_policy_from_config(cfg, camera).is_configured()
    }

    /// Returns `true` when raw untranscoded video (MP4 / HLS VOD) must be blocked (`404`) for `camera`.
    pub async fn blocks_untranscoded_video(&self, camera: &str) -> bool {
        self.camera_mask_policy(camera).await.is_configured()
    }

    /// Returns `Some(404 NOT_FOUND)` when `camera` has a configured (`Active` or `InvalidFailClosed`) privacy mask.
    pub async fn untranscoded_video_gate_response(&self, camera: &str) -> Option<Response> {
        self.blocks_untranscoded_video(camera)
            .await
            .then(|| StatusCode::NOT_FOUND.into_response())
    }

    /// Returns `Some(drawbox_vf)` only when `policy` is `Active` and compiles a valid FFmpeg `drawbox` filter.
    pub fn drawbox_filter_for_policy(policy: &PrivacyMaskPolicy) -> Option<String> {
        policy.ffmpeg_drawbox_filter().ok().flatten()
    }

    /// Applies `raw_mask` against `(detect_w, detect_h)` (falling back to `rgb` dimensions when `0`),
    /// returning `false` (fail-closed) if `rgb` is empty or `raw_mask` is `Disabled` / `InvalidFailClosed`.
    pub fn apply_mask_or_fail_closed(
        rgb: &mut RgbImage,
        raw_mask: &Value,
        detect_w: u32,
        detect_h: u32,
    ) -> bool {
        if rgb.width() == 0 || rgb.height() == 0 {
            return false;
        }
        let src_w = if detect_w > 0 {
            detect_w
        } else {
            rgb.width().max(1)
        };
        let src_h = if detect_h > 0 {
            detect_h
        } else {
            rgb.height().max(1)
        };
        let policy = PrivacyMaskPolicy::from_value(raw_mask, src_w, src_h);
        if !matches!(policy, PrivacyMaskPolicy::Active(_)) {
            return false;
        }
        policy.redact_rgb(rgb)
    }

    /// Synchronously redacts encoded image bytes under `policy` (or passes through untouched when `Disabled`).
    /// Malformed images or `InvalidFailClosed` policies fail closed to a matte-dark `64x36` placeholder.
    pub fn redact_image_bytes_with_policy(
        raw_bytes: &[u8],
        policy: &PrivacyMaskPolicy,
        prefer_jpeg: bool,
    ) -> (Vec<u8>, &'static str) {
        Self::redact_image_bytes_with_policy_and_crop(raw_bytes, policy, None, prefer_jpeg)
    }

    /// Synchronously redacts encoded image bytes under `policy`, optionally projecting full-frame
    /// normalized polygons into `norm_crop_xywh` (`[x, y, w, h]`) for cropped event thumbnails.
    pub fn redact_image_bytes_with_policy_and_crop(
        raw_bytes: &[u8],
        policy: &PrivacyMaskPolicy,
        norm_crop_xywh: Option<[f64; 4]>,
        prefer_jpeg: bool,
    ) -> (Vec<u8>, &'static str) {
        let default_mime = if prefer_jpeg {
            "image/jpeg"
        } else {
            "image/webp"
        };
        if !policy.is_configured() {
            return (raw_bytes.to_vec(), default_mime);
        }
        let Some(mut rgb) = decode_bounded_snapshot_rgb(raw_bytes) else {
            return encode_dark_placeholder(prefer_jpeg);
        };
        let ok = match norm_crop_xywh {
            Some(crop) => policy.redact_rgb_crop(&mut rgb, crop),
            None => policy.redact_rgb(&mut rgb),
        };
        if !ok {
            return encode_dark_placeholder(prefer_jpeg);
        }
        if prefer_jpeg {
            let out = ninjanvr_vision::encode_image_jpeg(&rgb)
                .unwrap_or_else(|_| encode_dark_placeholder(true).0);
            (out, "image/jpeg")
        } else {
            let out = ninjanvr_vision::encode_image_webp(&rgb)
                .unwrap_or_else(|_| b"RIFF\x1a\x00\x00\x00WEBPVP8 ".to_vec());
            (out, "image/webp")
        }
    }

    /// Asynchronous `spawn_blocking` wrapper around [`Self::redact_image_bytes_with_policy`].
    pub async fn redact_image_bytes_with_policy_async(
        raw_bytes: Vec<u8>,
        policy: PrivacyMaskPolicy,
        prefer_jpeg: bool,
    ) -> (Vec<u8>, &'static str) {
        let default_mime = if prefer_jpeg {
            "image/jpeg"
        } else {
            "image/webp"
        };
        if !policy.is_configured() {
            return (raw_bytes, default_mime);
        }
        tokio::task::spawn_blocking(move || {
            Self::redact_image_bytes_with_policy(&raw_bytes, &policy, prefer_jpeg)
        })
        .await
        .unwrap_or_else(|_| encode_dark_placeholder(prefer_jpeg))
    }

    /// Looks up `camera`'s [`PrivacyMaskPolicy`] and redacts `raw_bytes` off the async executor.
    pub async fn redact_image_bytes_for_camera(
        &self,
        camera: &str,
        raw_bytes: Vec<u8>,
        prefer_jpeg: bool,
    ) -> (Vec<u8>, &'static str) {
        let policy = self.camera_mask_policy(camera).await;
        Self::redact_image_bytes_with_policy_async(raw_bytes, policy, prefer_jpeg).await
    }

    /// Redacts encoded image bytes from an optional JSON `privacy_mask` value.
    pub fn redact_image_bytes_with_mask(
        raw_bytes: &[u8],
        privacy_mask: Option<&Value>,
        prefer_jpeg: bool,
    ) -> (Vec<u8>, &'static str) {
        let default_mime = if prefer_jpeg {
            "image/jpeg"
        } else {
            "image/webp"
        };
        let Some(mask_val) = privacy_mask else {
            return (raw_bytes.to_vec(), default_mime);
        };
        // Check disabled before decoding image bytes
        let probe = PrivacyMaskPolicy::from_value(mask_val, 0, 0);
        if !probe.is_configured() {
            return (raw_bytes.to_vec(), default_mime);
        }
        if probe.is_invalid() {
            return encode_dark_placeholder(prefer_jpeg);
        }
        let Some(mut rgb) = decode_bounded_snapshot_rgb(raw_bytes) else {
            return encode_dark_placeholder(prefer_jpeg);
        };
        let policy =
            PrivacyMaskPolicy::from_value(mask_val, rgb.width().max(1), rgb.height().max(1));
        if !policy.redact_rgb(&mut rgb) {
            return encode_dark_placeholder(prefer_jpeg);
        }
        if prefer_jpeg {
            let out = ninjanvr_vision::encode_image_jpeg(&rgb)
                .unwrap_or_else(|_| encode_dark_placeholder(true).0);
            (out, "image/jpeg")
        } else {
            let out = ninjanvr_vision::encode_image_webp(&rgb)
                .unwrap_or_else(|_| b"RIFF\x1a\x00\x00\x00WEBPVP8 ".to_vec());
            (out, "image/webp")
        }
    }

    /// Asynchronous `spawn_blocking` wrapper around [`Self::redact_image_bytes_with_mask`].
    pub async fn redact_image_bytes_with_mask_async(
        raw_bytes: Vec<u8>,
        privacy_mask: Option<Value>,
        prefer_jpeg: bool,
    ) -> (Vec<u8>, &'static str) {
        let default_mime = if prefer_jpeg {
            "image/jpeg"
        } else {
            "image/webp"
        };
        let Some(mask_val) = privacy_mask else {
            return (raw_bytes, default_mime);
        };
        if !PrivacyMaskPolicy::from_value(&mask_val, 0, 0).is_configured() {
            return (raw_bytes, default_mime);
        }
        tokio::task::spawn_blocking(move || {
            Self::redact_image_bytes_with_mask(&raw_bytes, Some(&mask_val), prefer_jpeg)
        })
        .await
        .unwrap_or_else(|_| encode_dark_placeholder(prefer_jpeg))
    }

    /// Redacts base64 `thumbnail` fields in-place across a slice of event JSON records for all
    /// cameras with an active or fail-closed privacy mask.
    pub async fn redact_event_slice_thumbnails(&self, events: &mut [Value]) {
        if events.is_empty() {
            return;
        }
        let active_policies = {
            let cfg = self.config.read().await;
            Self::configured_camera_policies(&cfg)
        };
        if active_policies.is_empty() {
            return;
        }
        let needs_redaction = events.iter().any(|ev| {
            ev.get("camera")
                .and_then(Value::as_str)
                .is_some_and(|cam| active_policies.contains_key(cam))
                && ev
                    .get("thumbnail")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty())
        });
        if !needs_redaction {
            return;
        }
        let mut owned_events: Vec<Value> = events
            .iter_mut()
            .map(|ev| std::mem::replace(ev, Value::Null))
            .collect();
        if let Ok(redacted_events) = tokio::task::spawn_blocking(move || {
            for ev in &mut owned_events {
                let Some(cam) = ev.get("camera").and_then(Value::as_str) else {
                    continue;
                };
                let Some(policy) = active_policies.get(cam) else {
                    continue;
                };
                let Some(b64) = ev
                    .get("thumbnail")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let raw_bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .unwrap_or_default();
                let crop_xywh = extract_event_crop_xywh(ev);
                let (redacted, _) = Self::redact_image_bytes_with_policy_and_crop(
                    &raw_bytes, policy, crop_xywh, false,
                );
                if let Some(obj) = ev.as_object_mut() {
                    obj.insert(
                        "thumbnail".to_string(),
                        Value::String(base64::engine::general_purpose::STANDARD.encode(redacted)),
                    );
                }
            }
            owned_events
        })
        .await
        {
            for (dst, src) in events.iter_mut().zip(redacted_events) {
                *dst = src;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn media_delivery_gate_redacts_2pt_bbox_and_fails_closed_on_malformed_mask() {
        let yaml = r#"
cameras:
  front_door:
    enabled: true
    detect:
      width: 640
      height: 360
    privacy_mask: "160,90,480,270"
  broken_cam:
    enabled: true
    detect:
      width: 640
      height: 360
    privacy_mask: ["not_a_polygon"]
  clear_cam:
    enabled: true
"#;
        let cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        let gate = MediaDeliveryGate::new(Arc::new(RwLock::new(cfg)));

        assert!(gate.blocks_untranscoded_video("front_door").await);
        assert!(gate.blocks_untranscoded_video("broken_cam").await);
        assert!(!gate.blocks_untranscoded_video("clear_cam").await);

        let bright = RgbImage::from_pixel(320, 180, Rgb([230, 230, 230]));
        let bright_webp = ninjanvr_vision::encode_image_webp(&bright).unwrap();

        // 1. 2-point detect-space box ("160,90,480,270" on 640x360) scales proportionally onto 320x180
        let (masked_bytes, _) = gate
            .redact_image_bytes_for_camera("front_door", bright_webp.clone(), false)
            .await;
        let masked_rgb = decode_bounded_snapshot_rgb(&masked_bytes).unwrap();
        assert!(masked_rgb.get_pixel(160, 90)[0] < 30);
        assert!(masked_rgb.get_pixel(10, 10)[0] > 200);

        // 2. Malformed mask fails closed to dark placeholder
        let (fc_bytes, _) = gate
            .redact_image_bytes_for_camera("broken_cam", bright_webp.clone(), true)
            .await;
        let fc_rgb = decode_bounded_snapshot_rgb(&fc_bytes).unwrap();
        assert!(fc_rgb.get_pixel(32, 18)[0] < 30);

        // 3. Unmasked camera returns original bytes untouched
        let (clear_bytes, _) = gate
            .redact_image_bytes_for_camera("clear_cam", bright_webp.clone(), false)
            .await;
        assert_eq!(clear_bytes, bright_webp);

        // 4. Cropped event thumbnails project full-frame privacy_mask into ev["data"]["region"]
        //    front_door mask is "160,90,480,270" on 640x360 => normalized [0.25..0.75, 0.25..0.75].
        //    Suppose crop region is [0.5, 0.5, 0.5, 0.5] => mask covers [0.0..0.5, 0.0..0.5] of the crop,
        //    leaving [0.5..1.0, 0.5..1.0] of the crop unmasked!
        let crop_bright = RgbImage::from_pixel(100, 100, Rgb([235, 235, 235]));
        let crop_b64 = base64::engine::general_purpose::STANDARD
            .encode(ninjanvr_vision::encode_image_webp(&crop_bright).unwrap());
        let mut evs = vec![
            serde_json::json!({
                "id": "ev_cropped_1",
                "camera": "front_door",
                "thumbnail": crop_b64,
                "data": { "region": [0.5, 0.5, 0.5, 0.5] }
            }),
            serde_json::json!({
                "id": "ev_no_region_fallback",
                "camera": "front_door",
                "thumbnail": crop_b64,
                "data": {}
            }),
        ];
        gate.redact_event_slice_thumbnails(&mut evs).await;

        let dec_cropped = decode_bounded_snapshot_rgb(
            &base64::engine::general_purpose::STANDARD
                .decode(evs[0]["thumbnail"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(
            dec_cropped.get_pixel(25, 25)[0] < 30,
            "top-left quadrant of [0.5..1.0] crop intersects [0.25..0.75] mask and must be redacted"
        );
        assert!(
            dec_cropped.get_pixel(80, 80)[0] > 200,
            "bottom-right quadrant of [0.5..1.0] crop is outside [0.25..0.75] mask and must remain visible"
        );

        let dec_fallback = decode_bounded_snapshot_rgb(
            &base64::engine::general_purpose::STANDARD
                .decode(evs[1]["thumbnail"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(
            dec_fallback.get_pixel(50, 50)[0] < 30,
            "event without region metadata must fall back to full-frame mask application"
        );
    }
}
