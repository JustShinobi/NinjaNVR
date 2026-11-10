use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use ninjanvr_db::{CameraScope, NinjaDatabase};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::share_engine::{ensure_path_within_canon_media_dir, ensure_path_within_media_dir};

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

/// Resolves a database recording path (which may carry a legacy `/media/ninjanvr/` prefix)
/// against the active `media_dir`.
pub fn resolve_recording_disk_path(media_dir: &Path, db_path: &str) -> PathBuf {
    if let Some(rel) = ninjanvr_db::strip_media_root(db_path) {
        media_dir.join(rel)
    } else {
        PathBuf::from(db_path)
    }
}

/// Verifies that `candidate` is a non-symlink file inside `media_dir/recordings` (and, when
/// `camera` is `Some`, belongs to that camera's directory hierarchy).
pub fn ensure_recording_segment_path(
    media_dir: &Path,
    camera: Option<&str>,
    candidate: &Path,
) -> Option<PathBuf> {
    let recordings_dir = media_dir.join("recordings");
    let canon_rec_dir = ensure_path_within_media_dir(media_dir, &recordings_dir)?;
    let safe_path = ensure_path_within_media_dir(&canon_rec_dir, candidate)?;
    if let Some(cam) = camera
        && let Ok(rel) = safe_path.strip_prefix(&canon_rec_dir)
        && let Some(parent) = rel.parent()
        && !parent.as_os_str().is_empty()
        && !parent.components().any(|c| c.as_os_str() == cam)
    {
        return None;
    }
    Some(safe_path)
}

/// Resolves a recording segment `db_path` across both the hot `media_dir/recordings` tier and
/// the optional `cold_dir` tier, enforcing non-symlink regular-file and camera containment.
pub fn resolve_contained_recording_segment(
    media_dir: &Path,
    cold_dir: Option<&Path>,
    camera: Option<&str>,
    db_path: &str,
) -> Option<PathBuf> {
    if db_path.is_empty()
        || Path::new(db_path)
            .components()
            .any(|c| matches!(c, Component::ParentDir))
    {
        return None;
    }
    let disk_path = resolve_recording_disk_path(media_dir, db_path);
    if let Some(hot_path) = ensure_recording_segment_path(media_dir, camera, &disk_path)
        && hot_path.is_file()
        && hot_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
    {
        return Some(hot_path);
    }

    let cold_root = cold_dir?;
    if std::fs::symlink_metadata(cold_root).is_ok_and(|m| m.file_type().is_symlink()) {
        return None;
    }
    let canon_cold = std::fs::canonicalize(cold_root).ok()?;
    let mut cold_candidates = vec![disk_path];
    if let Some(rel) = ninjanvr_db::strip_media_root(db_path) {
        let rel_stripped = rel.strip_prefix("recordings/").unwrap_or(rel);
        cold_candidates.push(cold_root.join(rel_stripped));
        cold_candidates.push(cold_root.join(rel));
    }
    for cand in cold_candidates {
        let Some(safe_path) = ensure_path_within_canon_media_dir(&canon_cold, &cand) else {
            continue;
        };
        if !safe_path.is_file()
            || !safe_path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
        {
            continue;
        }
        if let Some(cam) = camera
            && let Ok(rel) = safe_path.strip_prefix(&canon_cold)
            && let Some(parent) = rel.parent()
            && !parent.as_os_str().is_empty()
            && !parent.components().any(|c| c.as_os_str() == cam)
        {
            continue;
        }
        return Some(safe_path);
    }
    None
}

/// Resolves safe, non-symlink export video and thumbnail paths inside `media_dir/exports`
/// and `media_dir/clips` respectively, protecting `latest_*` live snapshots.
pub fn resolve_safe_export_unlink_paths(
    media_dir: &Path,
    vpath: &str,
    tpath: &str,
) -> (Option<PathBuf>, Option<PathBuf>) {
    let same_regular_inode = |raw_p: &Path, canon_p: &Path| -> bool {
        let Ok(lmeta) = std::fs::symlink_metadata(raw_p) else {
            return false;
        };
        if !lmeta.file_type().is_file() {
            return false;
        }
        let Ok(cmeta) = std::fs::symlink_metadata(canon_p) else {
            return false;
        };
        if !cmeta.file_type().is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if lmeta.dev() != cmeta.dev() || lmeta.ino() != cmeta.ino() {
                return false;
            }
        }
        true
    };
    let raw_vpath = resolve_recording_disk_path(media_dir, vpath);
    let safe_vpath = if std::fs::symlink_metadata(&raw_vpath).is_ok_and(|m| m.file_type().is_file())
    {
        ensure_path_within_media_dir(media_dir, &raw_vpath)
            .filter(|canon_v| {
                same_regular_inode(&raw_vpath, canon_v)
                    && std::fs::canonicalize(media_dir.join("exports"))
                        .ok()
                        .is_some_and(|exports_canon| {
                            canon_v.parent() == Some(exports_canon.as_path())
                        })
            })
            .map(|_| raw_vpath)
    } else {
        None
    };

    let raw_tpath = resolve_recording_disk_path(media_dir, tpath);
    let safe_tpath = if std::fs::symlink_metadata(&raw_tpath).is_ok_and(|m| m.file_type().is_file())
    {
        ensure_path_within_media_dir(media_dir, &raw_tpath)
            .filter(|canon_t| {
                let not_latest = !canon_t
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("latest_"));
                not_latest
                    && same_regular_inode(&raw_tpath, canon_t)
                    && std::fs::canonicalize(media_dir.join("clips"))
                        .ok()
                        .is_some_and(|clips_canon| canon_t.parent() == Some(clips_canon.as_path()))
            })
            .map(|_| raw_tpath)
    } else {
        None
    };

    (safe_vpath, safe_tpath)
}

/// Atomically writes `bytes` to `target_dir.join(filename)` after verifying that `media_dir`,
/// `target_dir`, and the target file are not symlinks and that `target_dir` resolves inside `media_dir`.
pub fn write_contained_media_file(
    media_dir: &Path,
    target_dir: &Path,
    filename: &str,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    use std::io::{Error, ErrorKind, Write};
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
            Component::Normal(part) => {
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
    let nonce: u64 = rand::random();
    let tmp_name = format!(".{filename}.{nonce:016x}.tmp");
    let tmp_path = target_dir.join(tmp_name);
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
    drop(tmp_file);
    if let Ok(dst_meta) = std::fs::symlink_metadata(&dst_path)
        && (dst_meta.file_type().is_symlink() || !dst_meta.file_type().is_file())
    {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "refusing to rename over symlink or non-regular file in media_dir",
        ));
    }
    if let Err(err) = std::fs::rename(&tmp_path, &dst_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(dst_path)
}

/// Async helper that atomically writes `bytes` into `media_dir.join(subdir_name).join(filename)`
/// on the blocking threadpool while enforcing non-symlink directory and file containment.
pub async fn atomic_write_within_media_subdir(
    media_dir: &Path,
    subdir_name: &str,
    filename: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    if !is_safe_id(filename) || filename.contains('/') || filename.contains('\\') {
        anyhow::bail!("Invalid filename '{filename}'");
    }
    let media_dir_owned = media_dir.to_path_buf();
    let subdir_owned = subdir_name.to_string();
    let filename_owned = filename.to_string();
    let bytes_owned = bytes.to_vec();
    tokio::task::spawn_blocking(move || -> Result<PathBuf> {
        let subdir = media_dir_owned.join(&subdir_owned);
        let written =
            write_contained_media_file(&media_dir_owned, &subdir, &filename_owned, &bytes_owned)?;
        let Some(canon_subdir) = ensure_path_within_media_dir(&media_dir_owned, &subdir) else {
            let _ = std::fs::remove_file(&written);
            anyhow::bail!("Target directory '{subdir_owned}' escapes media_dir");
        };
        let Some(safe_final) = ensure_path_within_media_dir(&canon_subdir, &written) else {
            let _ = std::fs::remove_file(&written);
            anyhow::bail!("Written file escapes '{subdir_owned}' directory");
        };
        Ok(safe_final)
    })
    .await?
}

/// Deletes a single event from SQLite and cleans up all associated snapshots, clips,
/// custom `thumb_path` files, and share-link artifacts under `media_dir`.
pub fn delete_event_with_artifacts(
    db: &NinjaDatabase,
    media_dir: &Path,
    event_id: &str,
) -> Result<()> {
    let existing_env = db
        .get_event(event_id)
        .ok()
        .flatten()
        .map(|ev| extract_event_envelope(&ev));
    db.delete_event(event_id)?;
    let cam_opt = existing_env
        .as_ref()
        .map(|e| e.camera.as_str())
        .filter(|s| !s.is_empty());
    let thumb_opt = existing_env.as_ref().and_then(|e| e.thumb_path.as_deref());
    cleanup_event_media_artifacts(media_dir, event_id, cam_opt, thumb_opt);
    Ok(())
}

/// Deletes a batch of events scoped to `scope` and cleans up all associated media artifacts.
pub fn delete_events_bulk_with_artifacts(
    db: &NinjaDatabase,
    media_dir: &Path,
    ids: &[String],
    scope: &CameraScope,
) -> Result<std::result::Result<usize, String>> {
    match db.delete_events_bulk_scoped(ids, scope)? {
        Ok(deleted_items) => {
            for (id, cam, thumb_opt) in &deleted_items {
                cleanup_event_media_artifacts(
                    media_dir,
                    id,
                    Some(cam.as_str()),
                    thumb_opt.as_deref(),
                );
            }
            for id in ids {
                if !deleted_items.iter().any(|(d_id, _, _)| d_id == id) {
                    cleanup_event_media_artifacts(media_dir, id, None, None);
                }
            }
            Ok(Ok(deleted_items.len()))
        }
        Err(forbidden_cam) => Ok(Err(forbidden_cam)),
    }
}

/// Deletes one or more review segments scoped to `scope` and removes all associated review
/// thumbnails, custom `thumb_path` files, and share-link artifacts under `media_dir`.
pub fn delete_reviews_with_artifacts(
    db: &NinjaDatabase,
    media_dir: &Path,
    ids: &[String],
    scope: &CameraScope,
) -> Result<std::result::Result<u64, String>> {
    match db.delete_reviews_scoped(ids, scope)? {
        Ok((deleted_count, thumbs)) => {
            for id in ids {
                cleanup_review_media_artifacts(media_dir, id, None);
            }
            for thumb in thumbs.iter().flatten() {
                cleanup_review_media_artifacts(media_dir, "", Some(thumb.as_str()));
            }
            Ok(Ok(deleted_count))
        }
        Err(forbidden_cam) => Ok(Err(forbidden_cam)),
    }
}

/// Deletes an export row from SQLite and removes its contained MP4, thumbnail, and share-link artifacts.
pub fn delete_export_with_artifacts(
    db: &NinjaDatabase,
    media_dir: &Path,
    export_id: &str,
) -> Result<bool> {
    cleanup_share_clip_artifacts_in_dir(media_dir, "export", export_id);
    if let Some((vpath, tpath)) = db.delete_export(export_id)? {
        let (safe_vpath, safe_tpath) = resolve_safe_export_unlink_paths(media_dir, &vpath, &tpath);
        if let Some(vp) = safe_vpath {
            let _ = std::fs::remove_file(vp);
        }
        if let Some(tp) = safe_tpath {
            let _ = std::fs::remove_file(tp);
        }
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Deletes recording segments for `camera` in `[after, before]` across both hot `media_dir/recordings`
/// and optional `cold_dir`, pruning empty parent directories and returning `(deleted_count, freed_bytes)`.
pub fn delete_recordings_in_range_with_artifacts(
    db: &NinjaDatabase,
    media_dir: &Path,
    cold_dir: Option<&Path>,
    camera: &str,
    after: f64,
    before: f64,
) -> Result<(usize, u64)> {
    let deleted = db.delete_recordings_in_range(camera, after, before)?;
    let recordings_root = media_dir.join("recordings");
    let canon_rec_root = std::fs::canonicalize(&recordings_root).ok();
    let canon_cold_root = cold_dir.and_then(|c| std::fs::canonicalize(c).ok());
    let mut freed_bytes: u64 = 0;
    for (_id, db_path, seg_size_mb) in &deleted {
        if let Some(safe_disk_path) =
            resolve_contained_recording_segment(media_dir, cold_dir, Some(camera), db_path)
        {
            if let Ok(meta) = std::fs::metadata(&safe_disk_path) {
                freed_bytes += meta.len();
            } else {
                freed_bytes += (seg_size_mb * 1024.0 * 1024.0).round() as u64;
            }
            let _ = std::fs::remove_file(&safe_disk_path);
            if let Some(ref canon_root) = canon_rec_root
                && safe_disk_path.starts_with(canon_root)
            {
                ninjanvr_media::prune_empty_parent_dirs(&safe_disk_path, canon_root);
            } else if let Some(ref canon_cold) = canon_cold_root
                && safe_disk_path.starts_with(canon_cold)
            {
                ninjanvr_media::prune_empty_parent_dirs(&safe_disk_path, canon_cold);
            }
        } else {
            freed_bytes += (seg_size_mb * 1024.0 * 1024.0).round() as u64;
        }
    }
    Ok((deleted.len(), freed_bytes))
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

    #[test]
    fn single_and_bulk_delete_helpers_clean_artifacts_and_resolve_cold_segments() {
        use ninjanvr_db::{EventDto, ReviewItemDto};

        let tmp_dir = std::env::temp_dir().join(format!(
            "ninjanvr_media_vault_test_{}_{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let cold_dir = std::env::temp_dir().join(format!(
            "ninjanvr_media_vault_cold_{}_{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let _ = std::fs::remove_dir_all(&cold_dir);

        let clips_dir = tmp_dir.join("clips");
        let review_clips_dir = clips_dir.join("review");
        let exports_dir = tmp_dir.join("exports");
        let hot_cam_dir = tmp_dir.join("recordings").join("front_door");
        let cold_cam_dir = cold_dir.join("front_door");
        std::fs::create_dir_all(&review_clips_dir).unwrap();
        std::fs::create_dir_all(&exports_dir).unwrap();
        std::fs::create_dir_all(&hot_cam_dir).unwrap();
        std::fs::create_dir_all(&cold_cam_dir).unwrap();

        let db = NinjaDatabase::open(tmp_dir.join("vault.db")).unwrap();

        // 1. Single event delete cleans up custom thumb_path + share artifacts
        let ev_dto = EventDto {
            id: "ev_single_1".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            sub_label: None,
            top_score: Some(0.92),
            false_positive: Some(false),
            zones: json!([]),
            thumbnail: Some("/media/ninjanvr/clips/custom_ev_single_1.webp".to_string()),
            start_time: 1000.0,
            end_time: Some(1005.0),
            has_clip: true,
            has_snapshot: true,
            retain_indefinitely: false,
            plus_id: None,
            box_coords: None,
            area: None,
            ratio: None,
            score: Some(0.92),
            stationary: false,
            data: json!({"thumb_path": "/media/ninjanvr/clips/custom_ev_single_1.webp"}),
        };
        db.upsert_event(&ev_dto).unwrap();
        let custom_ev_thumb = clips_dir.join("custom_ev_single_1.webp");
        let share_ev_clip = exports_dir.join("share_redacted_ev_single_1.mp4");
        std::fs::write(&custom_ev_thumb, b"thumb").unwrap();
        std::fs::write(&share_ev_clip, b"share").unwrap();

        delete_event_with_artifacts(&db, &tmp_dir, "ev_single_1").unwrap();
        assert!(db.get_event("ev_single_1").unwrap().is_none());
        assert!(!custom_ev_thumb.exists());
        assert!(!share_ev_clip.exists());

        // 2. Single review delete via delete_reviews_with_artifacts cleans up review thumbnail
        let rev_dto = ReviewItemDto {
            id: "rev_single_1".to_string(),
            camera: "front_door".to_string(),
            start_time: 1000.0,
            end_time: Some(1010.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-rev_single_1.webp".to_string(),
            data: json!({"objects": ["person"]}),
            has_been_reviewed: false,
        };
        db.insert_review_segment(&rev_dto).unwrap();
        let rev_thumb = review_clips_dir.join("thumb-rev_single_1.webp");
        std::fs::write(&rev_thumb, b"rev-thumb").unwrap();

        let del_rev = delete_reviews_with_artifacts(
            &db,
            &tmp_dir,
            &["rev_single_1".to_string()],
            &CameraScope::All,
        )
        .unwrap()
        .unwrap();
        assert_eq!(del_rev, 1);
        assert!(!rev_thumb.exists());

        // 3. Hot + cold recording segment resolution and range deletion
        let hot_seg = hot_cam_dir.join("seg_hot.mp4");
        let cold_seg = cold_cam_dir.join("seg_cold.mp4");
        std::fs::write(&hot_seg, b"0123456789").unwrap();
        std::fs::write(&cold_seg, b"0123456789abcdef").unwrap();

        assert_eq!(
            resolve_contained_recording_segment(
                &tmp_dir,
                Some(&cold_dir),
                Some("front_door"),
                "/media/ninjanvr/recordings/front_door/seg_hot.mp4",
            ),
            Some(std::fs::canonicalize(&hot_seg).unwrap())
        );
        assert_eq!(
            resolve_contained_recording_segment(
                &tmp_dir,
                Some(&cold_dir),
                Some("front_door"),
                &cold_seg.to_string_lossy(),
            ),
            Some(std::fs::canonicalize(&cold_seg).unwrap())
        );
        assert!(
            resolve_contained_recording_segment(
                &tmp_dir,
                Some(&cold_dir),
                Some("other_cam"),
                &cold_seg.to_string_lossy(),
            )
            .is_none()
        );

        db.insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_hot".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/front_door/seg_hot.mp4".to_string(),
            start_time: 100.0,
            end_time: 110.0,
            duration: 10.0,
            motion: 1,
            objects: 1,
            dbfs: -20,
            segment_size: 0.01,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
        db.insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_cold".to_string(),
            camera: "front_door".to_string(),
            path: cold_seg.to_string_lossy().to_string(),
            start_time: 110.0,
            end_time: 120.0,
            duration: 10.0,
            motion: 1,
            objects: 1,
            dbfs: -20,
            segment_size: 0.01,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

        let (deleted_count, freed_bytes) = delete_recordings_in_range_with_artifacts(
            &db,
            &tmp_dir,
            Some(&cold_dir),
            "front_door",
            90.0,
            130.0,
        )
        .unwrap();
        assert_eq!(deleted_count, 2);
        assert_eq!(freed_bytes, 26);
        assert!(!hot_seg.exists());
        assert!(!cold_seg.exists());

        let _ = std::fs::remove_dir_all(&tmp_dir);
        let _ = std::fs::remove_dir_all(&cold_dir);
    }
}
