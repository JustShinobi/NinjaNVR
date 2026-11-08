use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Normalized, typed view of an event JSON envelope across legacy top-level columns
/// and nested `data.*` / `data.provenance.*` fallback shapes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedEventEnvelope {
    pub id: String,
    pub camera: String,
    pub label: String,
    pub top_score: Option<f64>,
    pub norm_bbox: Option<[f64; 4]>,
    pub start_time: f64,
    pub end_time: Option<f64>,
    pub thumb_path: Option<String>,
}

impl NormalizedEventEnvelope {
    pub fn top_score_or(&self, fallback: f64) -> f64 {
        self.top_score.unwrap_or(fallback)
    }

    pub fn norm_bbox_or_default(&self) -> [f64; 4] {
        self.norm_bbox.unwrap_or([0.0, 0.0, 0.0, 0.0])
    }
}

fn parse_four_f64(val: &Value) -> Option<[f64; 4]> {
    if let Some(arr) = val.as_array() {
        if arr.len() < 4 {
            return None;
        }
        let coords = [
            arr[0].as_f64()?,
            arr[1].as_f64()?,
            arr[2].as_f64()?,
            arr[3].as_f64()?,
        ];
        return coords.iter().all(|v| v.is_finite()).then_some(coords);
    }
    if let Some(obj) = val.as_object() {
        if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
            obj.get("x1").and_then(Value::as_f64),
            obj.get("y1").and_then(Value::as_f64),
            obj.get("x2").and_then(Value::as_f64),
            obj.get("y2").and_then(Value::as_f64),
        ) {
            let coords = [x1, y1, x2, y2];
            return coords.iter().all(|v| v.is_finite()).then_some(coords);
        }
        if let (Some(x), Some(y), Some(w), Some(h)) = (
            obj.get("x").and_then(Value::as_f64),
            obj.get("y").and_then(Value::as_f64),
            obj.get("w").and_then(Value::as_f64),
            obj.get("h").and_then(Value::as_f64),
        ) {
            let coords = [x, y, w, h];
            return coords.iter().all(|v| v.is_finite()).then_some(coords);
        }
    }
    None
}

fn non_empty_trimmed_str(val: &Value) -> Option<String> {
    val.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn looks_like_media_path(raw: &str) -> bool {
    let trimmed = raw.trim();
    !trimmed.is_empty()
        && !trimmed.contains('\0')
        && (trimmed.contains('/')
            || trimmed.ends_with(".webp")
            || trimmed.ends_with(".jpg")
            || trimmed.ends_with(".jpeg")
            || trimmed.ends_with(".png"))
}

/// Extracts a normalized [`NormalizedEventEnvelope`] from a top-level or nested `data.*` event JSON value.
pub fn extract_event_envelope(ev: &Value) -> NormalizedEventEnvelope {
    let data = &ev["data"];
    let provenance = &data["provenance"];

    let id = non_empty_trimmed_str(&ev["id"])
        .or_else(|| non_empty_trimmed_str(&data["id"]))
        .unwrap_or_default();

    let camera = non_empty_trimmed_str(&ev["camera"])
        .or_else(|| non_empty_trimmed_str(&data["camera"]))
        .unwrap_or_default();

    let label = non_empty_trimmed_str(&ev["label"])
        .or_else(|| non_empty_trimmed_str(&data["label"]))
        .or_else(|| {
            data["objects"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(non_empty_trimmed_str)
        })
        .unwrap_or_default();

    let top_score = ev["top_score"]
        .as_f64()
        .or_else(|| ev["score"].as_f64())
        .or_else(|| data["top_score"].as_f64())
        .or_else(|| data["score"].as_f64())
        .or_else(|| provenance["top_score"].as_f64())
        .or_else(|| provenance["score"].as_f64())
        .filter(|v| v.is_finite());

    let norm_bbox = parse_four_f64(&ev["norm_bbox"])
        .or_else(|| parse_four_f64(&ev["box"]))
        .or_else(|| parse_four_f64(&data["norm_bbox"]))
        .or_else(|| parse_four_f64(&data["box"]))
        .or_else(|| parse_four_f64(&provenance["norm_bbox"]))
        .or_else(|| parse_four_f64(&provenance["box"]));

    let start_time = ev["start_time"]
        .as_f64()
        .or_else(|| data["start_time"].as_f64())
        .filter(|v| v.is_finite())
        .unwrap_or(0.0);

    let end_time = ev["end_time"]
        .as_f64()
        .or_else(|| data["end_time"].as_f64())
        .filter(|v| v.is_finite());

    let thumb_path = non_empty_trimmed_str(&ev["thumb_path"])
        .or_else(|| non_empty_trimmed_str(&data["thumb_path"]))
        .or_else(|| non_empty_trimmed_str(&data["thumbnail_path"]))
        .or_else(|| non_empty_trimmed_str(&ev["thumbnail"]).filter(|s| looks_like_media_path(s)))
        .or_else(|| non_empty_trimmed_str(&data["thumbnail"]).filter(|s| looks_like_media_path(s)));

    NormalizedEventEnvelope {
        id,
        camera,
        label,
        top_score,
        norm_bbox,
        start_time,
        end_time,
        thumb_path,
    }
}

fn is_safe_id(id: &str) -> bool {
    let trimmed = id.trim();
    !trimmed.is_empty()
        && trimmed == id
        && !trimmed.contains("..")
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn has_only_normal_components(rel: &Path) -> bool {
    let mut count = 0usize;
    for comp in rel.components() {
        match comp {
            Component::Normal(os) => {
                let s = os.to_string_lossy();
                if s.is_empty() || s == "." || s == ".." {
                    return false;
                }
                count += 1;
            }
            _ => return false,
        }
    }
    count > 0
}

fn remove_non_symlink_under_media_dir(media_dir: &Path, rel: &Path) {
    if !has_only_normal_components(rel) {
        return;
    }
    if std::fs::symlink_metadata(media_dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return;
    }
    let mut current = media_dir.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for (idx, comp) in comps.iter().enumerate() {
        current.push(comp.as_os_str());
        let Ok(meta) = std::fs::symlink_metadata(&current) else {
            return;
        };
        if meta.file_type().is_symlink() {
            return;
        }
        if idx + 1 == comps.len() && meta.file_type().is_file() {
            let _ = std::fs::remove_file(&current);
        }
    }
}

fn resolve_relative_artifact_paths(media_dir: &Path, raw_path: &str) -> Vec<PathBuf> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() || trimmed.contains("..") || trimmed.contains('\0') {
        return Vec::new();
    }
    let mut out = Vec::new();
    let p = Path::new(trimmed);
    if let Ok(rel) = p.strip_prefix(media_dir)
        && has_only_normal_components(rel)
    {
        out.push(rel.to_path_buf());
    }
    if let Some(rel_str) = ninjanvr_db::strip_media_root(trimmed) {
        let rel = PathBuf::from(rel_str);
        if has_only_normal_components(&rel) && !out.contains(&rel) {
            out.push(rel);
        }
    }
    let without_leading = trimmed.trim_start_matches('/');
    if !without_leading.is_empty() {
        let rel = PathBuf::from(without_leading);
        if has_only_normal_components(&rel) {
            if (without_leading.starts_with("clips/")
                || without_leading.starts_with("exports/")
                || without_leading.starts_with("previews/"))
                && !out.contains(&rel)
            {
                out.push(rel);
            } else {
                let under_clips = PathBuf::from("clips").join(&rel);
                if has_only_normal_components(&under_clips) && !out.contains(&under_clips) {
                    out.push(under_clips);
                }
            }
        }
    }
    out
}

/// Removes cached share-link clip/snapshot artifacts for `target_id` inside `media_dir`.
pub fn cleanup_share_clip_artifacts_in_dir(media_dir: &Path, target_type: &str, target_id: &str) {
    if !is_safe_id(target_id) {
        return;
    }
    let exports_rel = Path::new("exports");
    let clips_rel = Path::new("clips");

    for name in [
        format!("share_redacted_{target_id}.mp4"),
        format!(".share_redacted_{target_id}.mask_hash"),
        format!("share_clip_{target_id}.mp4"),
    ] {
        remove_non_symlink_under_media_dir(media_dir, &exports_rel.join(name));
    }
    for name in [
        format!("share_redacted_{target_id}.webp"),
        format!("share_clip_{target_id}.webp"),
    ] {
        remove_non_symlink_under_media_dir(media_dir, &clips_rel.join(name));
    }

    let kind = target_type.trim();
    if is_safe_id(kind) {
        for name in [
            format!("share_redacted_{kind}_{target_id}.mp4"),
            format!(".share_redacted_{kind}_{target_id}.mask_hash"),
            format!("share_clip_{kind}_{target_id}.mp4"),
        ] {
            remove_non_symlink_under_media_dir(media_dir, &exports_rel.join(name));
        }
        for name in [
            format!("share_redacted_{kind}_{target_id}.webp"),
            format!("share_clip_{kind}_{target_id}.webp"),
        ] {
            remove_non_symlink_under_media_dir(media_dir, &clips_rel.join(name));
        }
    }
}

/// Removes event snapshot, clip, thumbnail, and share artifacts for `event_id` under `media_dir`.
pub fn cleanup_event_media_artifacts(
    media_dir: &Path,
    event_id: &str,
    camera: Option<&str>,
    thumb_path: Option<&str>,
) {
    if is_safe_id(event_id) {
        let clips_rel = Path::new("clips");
        let mut names = vec![
            format!("{event_id}.jpg"),
            format!("{event_id}-clean.jpg"),
            format!("{event_id}.webp"),
            format!("{event_id}-clean.webp"),
            format!("{event_id}.mp4"),
            format!("{event_id}-clean.mp4"),
        ];
        if let Some(cam) = camera.map(str::trim).filter(|c| is_safe_id(c)) {
            names.push(format!("{cam}-{event_id}.webp"));
            names.push(format!("{cam}-{event_id}-clean.webp"));
            names.push(format!("{cam}-{event_id}.jpg"));
            names.push(format!("{cam}-{event_id}-clean.jpg"));
            names.push(format!("{cam}-{event_id}.mp4"));
        }
        for name in names {
            remove_non_symlink_under_media_dir(media_dir, &clips_rel.join(name));
        }
        cleanup_share_clip_artifacts_in_dir(media_dir, "event", event_id);
    }

    if let Some(tp) = thumb_path {
        for rel in resolve_relative_artifact_paths(media_dir, tp) {
            remove_non_symlink_under_media_dir(media_dir, &rel);
        }
    }
}

/// Removes review segment thumbnail and share artifacts for `review_id` under `media_dir`.
pub fn cleanup_review_media_artifacts(media_dir: &Path, review_id: &str, thumb_path: Option<&str>) {
    if is_safe_id(review_id) {
        let clips_rel = Path::new("clips");
        let review_rel = clips_rel.join("review");
        for rel in [
            review_rel.join(format!("thumb-{review_id}.webp")),
            review_rel.join(format!("{review_id}.webp")),
            clips_rel.join(format!("review-{review_id}.webp")),
            clips_rel.join(format!("{review_id}.webp")),
            clips_rel.join(format!("{review_id}.jpg")),
        ] {
            remove_non_symlink_under_media_dir(media_dir, &rel);
        }
        cleanup_share_clip_artifacts_in_dir(media_dir, "review", review_id);
    }

    if let Some(tp) = thumb_path {
        for rel in resolve_relative_artifact_paths(media_dir, tp) {
            remove_non_symlink_under_media_dir(media_dir, &rel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_event_envelope_normalizes_legacy_top_level_and_nested_data_shapes() {
        // 1. Top-level canonical shape
        let top_ev = json!({
            "id": "ev_top_1",
            "camera": "front_door",
            "label": "person",
            "top_score": 0.94,
            "box": [0.1, 0.2, 0.5, 0.8],
            "start_time": 1700000100.5,
            "end_time": 1700000108.25,
            "thumbnail": "/media/ninjanvr/clips/front_door-ev_top_1.webp",
            "data": {
                "top_score": 0.50,
                "box": [0.0, 0.0, 0.1, 0.1]
            }
        });
        let env_top = extract_event_envelope(&top_ev);
        assert_eq!(env_top.id, "ev_top_1");
        assert_eq!(env_top.camera, "front_door");
        assert_eq!(env_top.label, "person");
        assert_eq!(env_top.top_score, Some(0.94));
        assert_eq!(env_top.norm_bbox, Some([0.1, 0.2, 0.5, 0.8]));
        assert_eq!(env_top.start_time, 1700000100.5);
        assert_eq!(env_top.end_time, Some(1700000108.25));
        assert_eq!(
            env_top.thumb_path.as_deref(),
            Some("/media/ninjanvr/clips/front_door-ev_top_1.webp")
        );

        // 2. Nested data + provenance fallback shape (and base64 thumbnail ignored as non-path)
        let nested_ev = json!({
            "thumbnail": "/9j/4AAQSkZJRgABAQAAAQABAAD",
            "data": {
                "id": "ev_nested_2",
                "camera": "backyard",
                "objects": ["car"],
                "start_time": 1700000200.0,
                "end_time": 1700000212.0,
                "thumb_path": "/clips/backyard-ev_nested_2.webp",
                "provenance": {
                    "top_score": 0.87,
                    "norm_bbox": [0.25, 0.30, 0.75, 0.90]
                }
            }
        });
        let env_nested = extract_event_envelope(&nested_ev);
        assert_eq!(env_nested.id, "ev_nested_2");
        assert_eq!(env_nested.camera, "backyard");
        assert_eq!(env_nested.label, "car");
        assert_eq!(env_nested.top_score, Some(0.87));
        assert_eq!(env_nested.norm_bbox, Some([0.25, 0.30, 0.75, 0.90]));
        assert_eq!(env_nested.start_time, 1700000200.0);
        assert_eq!(env_nested.end_time, Some(1700000212.0));
        assert_eq!(
            env_nested.thumb_path.as_deref(),
            Some("/clips/backyard-ev_nested_2.webp")
        );

        // 3. Empty/missing fields fall back safely
        let empty_env = extract_event_envelope(&json!({}));
        assert!(empty_env.id.is_empty());
        assert!(empty_env.camera.is_empty());
        assert!(empty_env.label.is_empty());
        assert_eq!(empty_env.top_score, None);
        assert_eq!(empty_env.top_score_or(0.85), 0.85);
        assert_eq!(empty_env.norm_bbox, None);
        assert_eq!(empty_env.norm_bbox_or_default(), [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(empty_env.start_time, 0.0);
        assert_eq!(empty_env.end_time, None);
        assert_eq!(empty_env.thumb_path, None);
    }

    #[test]
    fn cleanup_event_review_and_share_clip_artifacts_in_temp_media_dir() {
        let tmp_dir = std::env::temp_dir().join(format!(
            "ninjanvr_media_lifecycle_test_{}_{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let clips_dir = tmp_dir.join("clips");
        let review_clips_dir = clips_dir.join("review");
        let exports_dir = tmp_dir.join("exports");
        std::fs::create_dir_all(&review_clips_dir).unwrap();
        std::fs::create_dir_all(&exports_dir).unwrap();

        // Event artifacts + custom thumb_path + share artifacts
        let ev_files = [
            clips_dir.join("ev_100.jpg"),
            clips_dir.join("ev_100-clean.jpg"),
            clips_dir.join("ev_100.mp4"),
            clips_dir.join("front_door-ev_100.webp"),
            clips_dir.join("front_door-ev_100-clean.webp"),
            clips_dir.join("custom_ev_100_thumb.webp"),
            exports_dir.join("share_redacted_ev_100.mp4"),
            exports_dir.join(".share_redacted_ev_100.mask_hash"),
            exports_dir.join("share_clip_ev_100.mp4"),
            clips_dir.join("share_redacted_ev_100.webp"),
            clips_dir.join("share_clip_ev_100.webp"),
        ];
        for f in &ev_files {
            std::fs::write(f, b"artifact").unwrap();
        }
        let unrelated_file = clips_dir.join("front_door-ev_keep.webp");
        std::fs::write(&unrelated_file, b"keep").unwrap();

        cleanup_event_media_artifacts(
            &tmp_dir,
            "ev_100",
            Some("front_door"),
            Some("/media/ninjanvr/clips/custom_ev_100_thumb.webp"),
        );

        for f in &ev_files {
            assert!(!f.exists(), "expected event artifact {:?} to be removed", f);
        }
        assert!(unrelated_file.exists());

        // Review artifacts + thumb_path + share artifacts
        let rev_files = [
            review_clips_dir.join("thumb-rev_200.webp"),
            clips_dir.join("custom_rev_200.webp"),
            exports_dir.join("share_redacted_rev_200.mp4"),
            exports_dir.join(".share_redacted_rev_200.mask_hash"),
            exports_dir.join("share_clip_rev_200.mp4"),
            clips_dir.join("share_redacted_rev_200.webp"),
            clips_dir.join("share_clip_rev_200.webp"),
        ];
        for f in &rev_files {
            std::fs::write(f, b"review-artifact").unwrap();
        }

        cleanup_review_media_artifacts(&tmp_dir, "rev_200", Some("/clips/custom_rev_200.webp"));

        for f in &rev_files {
            assert!(
                !f.exists(),
                "expected review artifact {:?} to be removed",
                f
            );
        }

        // Traversal IDs/paths are rejected without removing unrelated files
        cleanup_event_media_artifacts(
            &tmp_dir,
            "../ev_keep",
            Some("front_door"),
            Some("../front_door-ev_keep.webp"),
        );
        assert!(unrelated_file.exists());

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
