#![forbid(unsafe_code)]
//! Deepened Face Identity Gallery & Sub-Label Catalog (`FaceIdentityCatalog`).
//!
//! ## Design-It-Twice Comparison (`/codebase-design`)
//!
//! - **Design A — Unified `FaceIdentityCatalog` Service + Pure Coordinate Projection (Selected):**
//!   Encapsulates privacy-masked 112x112 RGB face square extraction (`crop_event_face_square_from_bytes`,
//!   `load_event_face_sample_bytes_sync`), persistent sample mirroring and cleanup under
//!   `media_dir/clips/faces/<profile>/` (`mirror_face_sample_to_clips`,
//!   `remove_face_profile_from_clips_mirror`), historical person-event cosine similarity backfill
//!   (`backfill_face_gallery_events`), and camera-scoped profile/sub-label queries behind an
//!   `AppState`-owned `FaceIdentityCatalog`. All high-level mutations (`enroll_profile`,
//!   `delete_profile`, `assign_event_sub_label`, `list_scoped_profiles`) accept narrow
//!   `(db, detector, media_dir)` references so unit tests execute deterministically without
//!   spinning up HTTP servers.
//! - **Design B — Inline Handler Logic in `lib.rs` (Rejected):**
//!   Scatters coordinate projection, `clips/faces` mirror synchronization, and camera-scoped
//!   sub-label filtering across five HTTP route handlers in `lib.rs`, duplicating case-insensitive
//!   profile resolution and preventing isolated unit testing.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Json, Response};
use axum_extra::extract::cookie::CookieJar;
use base64::Engine;
use ninjanvr_db::{EventFilter, NinjaDatabase};
use ninjanvr_infer::{AiDetectorEngine, EnrolledFaceProfile};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::dataset_catalog::{
    MAX_DATASET_SAMPLE_FILE_BYTES, event_uses_xywh_norm_boxes, extract_event_box_array,
};
use crate::{
    AppState, caller_can_mutate, decode_bounded_snapshot_rgb, effective_caller_allowed_cameras,
    ensure_clips_artifact_path, is_safe_id, read_bounded_non_symlink_bytes_sync,
    redact_image_bytes_with_privacy_mask, unix_now, verify_cookie_csrf_origin,
};

#[derive(Debug, Deserialize, Default)]
pub struct SubLabelBody {
    #[serde(alias = "subLabel")]
    pub sub_label: Option<String>,
    #[serde(default, alias = "subLabelScore", alias = "score")]
    pub sub_label_score: Option<f64>,
    #[serde(default, alias = "enrollFace")]
    pub enroll_face: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
pub struct FaceEnrollBody {
    #[serde(default, alias = "eventId")]
    pub event_id: Option<String>,
    #[serde(default, alias = "imageBase64", alias = "image")]
    pub image_base64: Option<String>,
    #[serde(default, alias = "assignSubLabel")]
    pub assign_sub_label: Option<bool>,
    #[serde(default, alias = "backfillEvents")]
    pub backfill_events: Option<bool>,
}

/// Outcome of enrolling a face profile sample through [`FaceIdentityCatalog::enroll_profile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaceEnrollmentOutcome {
    pub canonical_name: String,
    pub sample_count: usize,
    pub embedding_dim: usize,
    pub backfilled_events: usize,
    pub saved_path: PathBuf,
}

/// Outcome of deleting a face profile through [`FaceIdentityCatalog::delete_profile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaceDeletionOutcome {
    pub canonical_name: String,
    pub deleted: bool,
    pub cleared_events: usize,
}

/// Outcome of assigning a sub-label to an event through [`FaceIdentityCatalog::assign_event_sub_label`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubLabelAssignmentOutcome {
    pub canonical_sub_label: Option<String>,
    pub updated: usize,
}

fn scope_face_cameras_filter(allowed: Option<&[String]>) -> Option<Vec<String>> {
    let allow_list = allowed?;
    if allow_list.iter().any(|a| a == "*") {
        return None;
    }
    let safe_allow: Vec<String> = allow_list
        .iter()
        .filter(|a| a.as_str() != "all")
        .cloned()
        .collect();
    Some(if safe_allow.is_empty() {
        vec!["__none__".to_string()]
    } else {
        safe_allow
    })
}

/// Projects a normalized event bounding box (`[x, y, w, h]` or `[x1, y1, x2, y2]`) into pixel
/// coordinates clamped within `(fw, fh)`.
pub fn norm_box_to_pixel_bbox(
    ev: &Value,
    raw: [f64; 4],
    fw: u32,
    fh: u32,
) -> Option<ninjanvr_vision::BoundingBox> {
    if fw == 0 || fh == 0 || raw.iter().all(|&v| v <= 0.0) {
        return None;
    }
    let use_xywh = event_uses_xywh_norm_boxes(ev) || !(raw[2] > raw[0] && raw[3] > raw[1]);
    let (xmin_n, ymin_n, xmax_n, ymax_n) = if use_xywh {
        let x0 = raw[0].clamp(0.0, 1.0);
        let y0 = raw[1].clamp(0.0, 1.0);
        let w = raw[2].clamp(0.001, (1.0 - x0).max(0.001));
        let h = raw[3].clamp(0.001, (1.0 - y0).max(0.001));
        (x0, y0, (x0 + w).clamp(0.0, 1.0), (y0 + h).clamp(0.0, 1.0))
    } else {
        let x0 = raw[0].min(raw[2]).clamp(0.0, 1.0);
        let y0 = raw[1].min(raw[3]).clamp(0.0, 1.0);
        let x1 = raw[0].max(raw[2]).clamp(0.0, 1.0);
        let y1 = raw[1].max(raw[3]).clamp(0.0, 1.0);
        (x0, y0, x1, y1)
    };
    let x_min = (xmin_n * (fw as f64)).round().clamp(0.0, (fw - 1) as f64) as u32;
    let y_min = (ymin_n * (fh as f64)).round().clamp(0.0, (fh - 1) as f64) as u32;
    let x_max = (xmax_n * (fw as f64))
        .round()
        .clamp((x_min + 1) as f64, fw as f64) as u32;
    let y_max = (ymax_n * (fh as f64))
        .round()
        .clamp((y_min + 1) as f64, fh as f64) as u32;
    Some(ninjanvr_vision::BoundingBox::new(
        x_min, y_min, x_max, y_max,
    ))
}

/// Extracts and encodes a 112x112 JPEG face square from `raw_image_bytes` using the event's
/// primary person box and optional `data.face_box` override.
pub fn crop_event_face_square_from_bytes(ev: &Value, raw_image_bytes: &[u8]) -> Option<Vec<u8>> {
    let raw_box = extract_event_box_array(ev);
    if raw_box.iter().all(|&v| v <= 0.0) {
        return None;
    }
    let rgb = decode_bounded_snapshot_rgb(raw_image_bytes)?;
    let fw = rgb.width();
    let fh = rgb.height();
    let person_bbox = norm_box_to_pixel_bbox(ev, raw_box, fw, fh)?;
    let face_attr_bbox = ev["data"]["face_box"]
        .as_array()
        .filter(|a| a.len() >= 4)
        .and_then(|a| {
            Some([
                a[0].as_f64()?,
                a[1].as_f64()?,
                a[2].as_f64()?,
                a[3].as_f64()?,
            ])
        })
        .and_then(|fb| norm_box_to_pixel_bbox(ev, fb, fw, fh));
    let crop_box = ninjanvr_infer::resolve_face_crop_bbox(person_bbox, face_attr_bbox, fw, fh, 1)
        .unwrap_or(person_bbox);
    let square = crate::pipeline::crop_and_scale_rgb_square(&rgb, crop_box, 112);
    ninjanvr_vision::encode_image_jpeg(&square).ok()
}

/// Mirrors a newly enrolled face sample file into `media_dir/clips/faces/<profile>/`.
pub fn mirror_face_sample_to_clips(media_dir: &Path, canonical_name: &str, saved_path: &Path) {
    let Some(safe_profile) = ninjanvr_infer::sanitize_face_profile_name(canonical_name) else {
        return;
    };
    let Some(fname) = saved_path.file_name().and_then(|s| s.to_str()) else {
        return;
    };
    let Some(bytes) =
        read_bounded_non_symlink_bytes_sync(saved_path, MAX_DATASET_SAMPLE_FILE_BYTES)
    else {
        return;
    };
    let target_dir = media_dir.join("clips").join("faces").join(safe_profile);
    let _ = crate::pipeline::write_contained_media_file(media_dir, &target_dir, fname, &bytes);
}

/// Removes all mirrored sample directories matching `profile_name` (case-insensitively) from
/// `media_dir/clips/faces/`.
pub fn remove_face_profile_from_clips_mirror(media_dir: &Path, profile_name: &str) {
    let Some(clean_name) = ninjanvr_infer::sanitize_face_profile_name(profile_name) else {
        return;
    };
    let mirror_root = media_dir.join("clips").join("faces");
    if !std::fs::symlink_metadata(&mirror_root).is_ok_and(|m| m.file_type().is_dir()) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&mirror_root) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_dir())
            && let Some(dir_name) = p.file_name().and_then(|s| s.to_str())
            && dir_name.eq_ignore_ascii_case(&clean_name)
            && ensure_clips_artifact_path(media_dir, &p).is_some()
        {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

/// Loads and privacy-redacts the 112x112 face sample bytes for `event_id` using `db` and `media_dir`.
pub fn load_event_face_sample_bytes_from_db_sync(
    db: &NinjaDatabase,
    media_dir: &Path,
    privacy_masks: &HashMap<String, Value>,
    event_id: &str,
) -> Vec<u8> {
    if !is_safe_id(event_id) {
        return format!("event-face-sample:{event_id}").into_bytes();
    }
    let clips_dir = media_dir.join("clips");
    if let Ok(Some(ev)) = db.get_event(event_id) {
        let cam_opt = ev["camera"].as_str().filter(|c| is_safe_id(c));
        let priv_mask = cam_opt.and_then(|cam| privacy_masks.get(cam));
        if let Some(cam) = cam_opt {
            for (candidate, is_jpeg) in [
                (
                    clips_dir.join(format!("{cam}-{event_id}-clean.webp")),
                    false,
                ),
                (clips_dir.join(format!("{event_id}-clean.jpg")), true),
                (clips_dir.join(format!("{cam}-{event_id}.webp")), false),
                (clips_dir.join(format!("{event_id}.jpg")), true),
            ] {
                if let Some(safe_p) = ensure_clips_artifact_path(media_dir, &candidate)
                    && let Some(bytes) =
                        read_bounded_non_symlink_bytes_sync(&safe_p, MAX_DATASET_SAMPLE_FILE_BYTES)
                    && !bytes.is_empty()
                {
                    let (out, _) = redact_image_bytes_with_privacy_mask(&bytes, priv_mask, is_jpeg);
                    if let Some(cropped) = crop_event_face_square_from_bytes(&ev, &out) {
                        return cropped;
                    }
                    return out;
                }
            }
        }
        if let Some(thumb_b64) = ev["thumbnail"].as_str()
            && let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(thumb_b64.trim())
            && !decoded.is_empty()
        {
            let (out, _) = redact_image_bytes_with_privacy_mask(&decoded, priv_mask, false);
            if let Some(cropped) = crop_event_face_square_from_bytes(&ev, &out) {
                return cropped;
            }
            return out;
        }
    }
    format!("event-face-sample:{event_id}").into_bytes()
}

/// Synchronously loads and privacy-redacts the 112x112 face sample bytes for `event_id`.
pub fn load_event_face_sample_bytes_sync(
    state: &AppState,
    privacy_masks: &HashMap<String, Value>,
    event_id: &str,
) -> Vec<u8> {
    load_event_face_sample_bytes_from_db_sync(&state.db, &state.media_dir, privacy_masks, event_id)
}

/// Asynchronously loads and privacy-redacts the 112x112 face sample bytes for `event_id`.
pub async fn load_event_face_sample_bytes(state: &AppState, event_id: &str) -> Vec<u8> {
    if !is_safe_id(event_id) {
        return format!("event-face-sample:{event_id}").into_bytes();
    }
    let masks: HashMap<String, Value> = {
        let cfg = state.config.read().await;
        cfg.cameras
            .iter()
            .map(|(k, v)| (k.clone(), v.privacy_mask.clone()))
            .collect()
    };
    let state_owned = state.clone();
    let event_id_owned = event_id.to_string();
    let fallback_id = event_id.to_string();
    tokio::task::spawn_blocking(move || {
        load_event_face_sample_bytes_sync(&state_owned, &masks, &event_id_owned)
    })
    .await
    .unwrap_or_else(|_| format!("event-face-sample:{fallback_id}").into_bytes())
}

/// Backfills unlabeled historical `person` events against the enrolled face gallery.
pub fn backfill_face_gallery_events_with_db(
    db: &NinjaDatabase,
    detector: &AiDetectorEngine,
    media_dir: &Path,
    faces_dir: &Path,
    similarity_threshold: f32,
    privacy_masks: &HashMap<String, Value>,
    exclude_event_id: Option<&str>,
) -> usize {
    let candidates = db
        .query_events(&EventFilter {
            labels: Some(vec!["person".to_string()]),
            limit: Some(500),
            ..Default::default()
        })
        .unwrap_or_default();
    let mut matched_count = 0usize;
    for ev in candidates {
        let Some(ev_id) = ev["id"].as_str().filter(|id| is_safe_id(id)) else {
            continue;
        };
        if exclude_event_id.is_some_and(|ex| ex == ev_id) {
            continue;
        }
        let existing_sub = ev["sub_label"]
            .as_str()
            .or_else(|| ev["data"]["sub_label"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("unknown"));
        if existing_sub.is_some() {
            continue;
        }
        let sample_bytes =
            load_event_face_sample_bytes_from_db_sync(db, media_dir, privacy_masks, ev_id);
        if let Some(m) =
            detector.match_face_crop(faces_dir, &sample_bytes, similarity_threshold, false)
            && m.is_known
            && db
                .set_event_sub_label_with_score(ev_id, Some(&m.sub_label), Some(m.score as f64))
                .unwrap_or(0)
                > 0
        {
            matched_count += 1;
        }
    }
    matched_count
}

/// Backfills unlabeled historical `person` events using `AppState`.
pub fn backfill_face_gallery_events(
    state: &AppState,
    faces_dir: &Path,
    similarity_threshold: f32,
    privacy_masks: &HashMap<String, Value>,
    exclude_event_id: Option<&str>,
) -> usize {
    backfill_face_gallery_events_with_db(
        &state.db,
        &state.detector,
        &state.media_dir,
        faces_dir,
        similarity_threshold,
        privacy_masks,
        exclude_event_id,
    )
}

/// Deep catalog encapsulating face identity enrollment, deletion, sub-label resolution,
/// `clips/faces` mirror management, and camera-scoped profile listing.
#[derive(Debug, Clone, Default)]
pub struct FaceIdentityCatalog;

impl FaceIdentityCatalog {
    pub const fn new() -> Self {
        Self
    }

    pub fn faces_dir(&self, media_dir: &Path) -> PathBuf {
        media_dir.join("faces")
    }

    /// Resolves `raw_sub_label` against enrolled face profiles (preserving canonical case),
    /// updates `event_id` in `db`, and optionally enrolls `enroll_sample_bytes` into the gallery.
    // Narrow dependency injection (db, detector, media_dir) keeps unit tests deterministic without AppState
    #[expect(clippy::too_many_arguments)]
    pub fn assign_event_sub_label(
        &self,
        db: &NinjaDatabase,
        detector: &AiDetectorEngine,
        media_dir: &Path,
        event_id: &str,
        raw_sub_label: Option<&str>,
        score: Option<f64>,
        enroll_sample_bytes: Option<&[u8]>,
    ) -> SubLabelAssignmentOutcome {
        let faces_dir = self.faces_dir(media_dir);
        let canonical = raw_sub_label
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                let profiles = detector.list_face_profiles(&faces_dir);
                profiles
                    .iter()
                    .find(|p| p.name.eq_ignore_ascii_case(s))
                    .map_or_else(|| s.to_string(), |p| p.name.clone())
            });
        let updated = db
            .set_event_sub_label_with_score(event_id, canonical.as_deref(), score)
            .unwrap_or(0);

        if updated > 0
            && let Some(sample_bytes) = enroll_sample_bytes
            && let Some(ref name) = canonical
            && !name.eq_ignore_ascii_case("unknown")
            && let Ok((saved_path, _)) =
                detector.enroll_face_profile(&faces_dir, name, event_id, sample_bytes)
        {
            mirror_face_sample_to_clips(media_dir, name, &saved_path);
        }

        SubLabelAssignmentOutcome {
            canonical_sub_label: canonical,
            updated,
        }
    }

    /// Enrolls a face profile sample, mirrors the sample into `clips/faces/<profile>/`,
    /// optionally assigns the canonical profile name to `event_to_assign`, and optionally
    /// backfills historical person events.
    // Narrow dependency injection (db, detector, media_dir) keeps unit tests deterministic without AppState
    #[expect(clippy::too_many_arguments)]
    pub fn enroll_profile(
        &self,
        db: &NinjaDatabase,
        detector: &AiDetectorEngine,
        media_dir: &Path,
        clean_name: &str,
        sample_id: &str,
        payload_bytes: &[u8],
        event_to_assign: Option<&str>,
        backfill_opts: Option<(f32, &HashMap<String, Value>)>,
    ) -> Result<FaceEnrollmentOutcome> {
        let faces_dir = self.faces_dir(media_dir);
        let (saved_path, emb) =
            detector.enroll_face_profile(&faces_dir, clean_name, sample_id, payload_bytes)?;
        let profiles = detector.list_face_profiles(&faces_dir);
        let matched_profile = profiles
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(clean_name));
        let canonical_name =
            matched_profile.map_or_else(|| clean_name.to_string(), |p| p.name.clone());
        let sample_count = matched_profile.map_or(1, |p| p.sample_count);

        mirror_face_sample_to_clips(media_dir, &canonical_name, &saved_path);

        if let Some(ev_id) = event_to_assign {
            let _ = db.set_event_sub_label_with_score(ev_id, Some(&canonical_name), Some(1.0));
        }

        let backfilled_events = if let Some((similarity_threshold, privacy_masks)) = backfill_opts {
            backfill_face_gallery_events_with_db(
                db,
                detector,
                media_dir,
                &faces_dir,
                similarity_threshold,
                privacy_masks,
                event_to_assign,
            )
        } else {
            0
        };

        Ok(FaceEnrollmentOutcome {
            canonical_name,
            sample_count,
            embedding_dim: emb.len(),
            backfilled_events,
            saved_path,
        })
    }

    /// Deletes a face profile from `faces_dir`, cleans up `clips/faces/<profile>/`, and clears
    /// matching event `sub_label`s in `db`.
    pub fn delete_profile(
        &self,
        db: &NinjaDatabase,
        detector: &AiDetectorEngine,
        media_dir: &Path,
        clean_name: &str,
    ) -> FaceDeletionOutcome {
        let faces_dir = self.faces_dir(media_dir);
        let deleted = detector
            .delete_face_profile(&faces_dir, clean_name)
            .unwrap_or(false);
        remove_face_profile_from_clips_mirror(media_dir, clean_name);
        let cleared_events = db.clear_face_profile_sub_labels(clean_name).unwrap_or(0);
        FaceDeletionOutcome {
            canonical_name: clean_name.to_string(),
            deleted,
            cleared_events,
        }
    }

    /// Lists enrolled face profiles filtered by the caller's permitted camera scope.
    pub fn list_scoped_profiles(
        &self,
        db: &NinjaDatabase,
        detector: &AiDetectorEngine,
        media_dir: &Path,
        allowed_cameras: Option<&[String]>,
    ) -> Vec<EnrolledFaceProfile> {
        let faces_dir = self.faces_dir(media_dir);
        let mut profiles = detector.list_face_profiles(&faces_dir);
        if let Some(allowed) = allowed_cameras
            && !allowed.iter().any(|a| a == "*")
        {
            let scoped_events = db
                .query_events(&EventFilter {
                    cameras: scope_face_cameras_filter(Some(allowed)),
                    limit: Some(1000),
                    ..Default::default()
                })
                .unwrap_or_default();
            let mut allowed_sub_labels = BTreeSet::new();
            for ev in &scoped_events {
                if let Some(sl) = ev["sub_label"]
                    .as_str()
                    .or_else(|| ev["data"]["sub_label"].as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    allowed_sub_labels.insert(sl.to_ascii_lowercase());
                }
            }
            for item in db.query_events_summary().unwrap_or_default() {
                if item["camera"]
                    .as_str()
                    .is_some_and(|cam| allowed.iter().any(|a| a == cam))
                    && let Some(sl) = item["sub_label"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                {
                    allowed_sub_labels.insert(sl.to_ascii_lowercase());
                }
            }
            profiles.retain_mut(|p| {
                p.sample_files.retain(|fname| {
                    let stem = fname.rsplit_once('.').map_or(fname.as_str(), |(s, _)| s);
                    match db.get_event(stem) {
                        Ok(Some(ev)) => ev["camera"]
                            .as_str()
                            .is_some_and(|cam| allowed.iter().any(|a| a == cam)),
                        _ => false,
                    }
                });
                p.sample_count = p.sample_files.len();
                allowed_sub_labels.contains(&p.name.to_ascii_lowercase())
                    || !p.sample_files.is_empty()
            });
        }
        profiles
    }

    /// Lists distinct sub-labels visible to the caller's permitted camera scope.
    pub fn list_scoped_sub_labels(
        &self,
        db: &NinjaDatabase,
        detector: &AiDetectorEngine,
        media_dir: &Path,
        allowed_cameras: Option<&[String]>,
    ) -> Vec<String> {
        let mut set = BTreeSet::new();
        if let Some(allowed) = allowed_cameras
            && !allowed.iter().any(|a| a == "*")
        {
            let scoped_events = db
                .query_events(&EventFilter {
                    cameras: scope_face_cameras_filter(Some(allowed)),
                    limit: Some(1000),
                    ..Default::default()
                })
                .unwrap_or_default();
            for ev in scoped_events {
                if let Some(sl) = ev["sub_label"]
                    .as_str()
                    .or_else(|| ev["data"]["sub_label"].as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    set.insert(sl.to_string());
                }
            }
            for item in db.query_events_summary().unwrap_or_default() {
                if item["camera"]
                    .as_str()
                    .is_some_and(|cam| allowed.iter().any(|a| a == cam))
                    && let Some(sl) = item["sub_label"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                {
                    set.insert(sl.to_string());
                }
            }
            return set.into_iter().collect();
        }
        if let Ok(db_subs) = db.list_distinct_sub_labels() {
            for s in db_subs {
                set.insert(s);
            }
        }
        let faces_dir = self.faces_dir(media_dir);
        for p in detector.list_face_profiles(&faces_dir) {
            set.insert(p.name);
        }
        set.into_iter().collect()
    }
}

pub async fn api_event_sub_label(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<SubLabelBody>,
) -> Response {
    if !verify_cookie_csrf_origin(&headers, &uri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    if !caller_can_mutate(&state, &jar, &headers, false).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Read-only role cannot modify events" })),
        )
            .into_response();
    }
    let allowed_opt = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    if let Some(ref allowed) = allowed_opt
        && !allowed.iter().any(|a| a == "*")
    {
        let permitted = state
            .db
            .get_event(&id)
            .ok()
            .flatten()
            .and_then(|ev| {
                ev["camera"]
                    .as_str()
                    .map(|cam| allowed.iter().any(|c| c == cam))
            })
            .unwrap_or(false);
        if !permitted {
            return (
                StatusCode::FORBIDDEN,
                Json(
                    json!({ "success": false, "message": "Camera is not permitted for this user" }),
                ),
            )
                .into_response();
        }
    }

    let is_unrestricted = allowed_opt
        .as_ref()
        .is_none_or(|a| a.iter().any(|c| c == "*"));
    let enroll_sample_bytes = if body.enroll_face.unwrap_or(false) && is_unrestricted {
        Some(load_event_face_sample_bytes(&state, &id).await)
    } else {
        None
    };

    let raw_sub = body.sub_label.clone();
    let score_for_sub = body.sub_label_score;
    let state_for_sub = state.clone();
    let id_for_sub = id.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        state_for_sub.face_gallery.assign_event_sub_label(
            &state_for_sub.db,
            &state_for_sub.detector,
            &state_for_sub.media_dir,
            &id_for_sub,
            raw_sub.as_deref(),
            score_for_sub,
            enroll_sample_bytes.as_deref(),
        )
    })
    .await
    .unwrap_or(SubLabelAssignmentOutcome {
        canonical_sub_label: None,
        updated: 0,
    });

    Json(json!({
        "success": true,
        "updated": outcome.updated,
        "sub_label": outcome.canonical_sub_label.as_deref(),
        "sub_label_score": body.sub_label_score
    }))
    .into_response()
}

pub async fn api_faces_list(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    let allowed_opt = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let state_for_list = state.clone();
    let root = tokio::task::spawn_blocking(move || {
        let profiles = state_for_list.face_gallery.list_scoped_profiles(
            &state_for_list.db,
            &state_for_list.detector,
            &state_for_list.media_dir,
            allowed_opt.as_deref(),
        );
        let mut faces_map = serde_json::Map::new();
        let mut root = serde_json::Map::new();
        for p in &profiles {
            faces_map.insert(p.name.clone(), json!(p.sample_files));
            if p.name != "profiles" && p.name != "faces" {
                root.insert(p.name.clone(), json!(p.sample_files));
            }
        }
        root.insert("profiles".to_string(), json!(profiles));
        root.insert("faces".to_string(), Value::Object(faces_map));
        root
    })
    .await
    .unwrap_or_default();
    Json(Value::Object(root))
}

pub async fn api_face_enroll(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
    body_bytes: Bytes,
) -> Response {
    if !verify_cookie_csrf_origin(&headers, &uri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    if !caller_can_mutate(&state, &jar, &headers, true).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Admin role required" })),
        )
            .into_response();
    }
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Camera-restricted users cannot modify global face gallery" })),
        )
            .into_response();
    }
    let Some(clean_name) = ninjanvr_infer::sanitize_face_profile_name(&name) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid face profile name" })),
        )
            .into_response();
    };

    let mut sample_id = format!("enroll_{}", unix_now());
    let mut event_to_assign: Option<String> = None;
    let mut should_backfill = false;
    let first_non_ws = body_bytes
        .iter()
        .copied()
        .find(|b| !b.is_ascii_whitespace());
    let payload_bytes: Vec<u8> = if body_bytes.is_empty() {
        format!("face-profile:{clean_name}").into_bytes()
    } else if first_non_ws == Some(b'{') {
        let Ok(parsed) = serde_json::from_slice::<FaceEnrollBody>(&body_bytes) else {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({ "success": false, "message": "Invalid JSON face enrollment payload" }),
                ),
            )
                .into_response();
        };
        should_backfill = parsed.backfill_events.unwrap_or(false);
        if let Some(ref ev_id) = parsed.event_id
            && !ev_id.trim().is_empty()
        {
            let trimmed_ev = ev_id.trim();
            if !is_safe_id(trimmed_ev) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "success": false, "message": "Invalid event_id" })),
                )
                    .into_response();
            }
            sample_id = trimmed_ev.to_string();
            if parsed.assign_sub_label.unwrap_or(true) {
                event_to_assign = Some(sample_id.clone());
            }
        }
        if let Some(ref b64) = parsed.image_base64
            && !b64.trim().is_empty()
        {
            let stripped = b64
                .trim()
                .split_once(";base64,")
                .map_or_else(|| b64.trim(), |(_, data)| data);
            base64::engine::general_purpose::STANDARD
                .decode(stripped)
                .unwrap_or_else(|_| stripped.as_bytes().to_vec())
        } else if let Some(ref ev_id) = parsed.event_id
            && !ev_id.trim().is_empty()
        {
            load_event_face_sample_bytes(&state, ev_id.trim()).await
        } else {
            format!("face-profile:{clean_name}").into_bytes()
        }
    } else {
        body_bytes.to_vec()
    };

    let (similarity_threshold, privacy_masks) = {
        let cfg = state.config.read().await;
        let masks: HashMap<String, Value> = cfg
            .cameras
            .iter()
            .map(|(k, v)| (k.clone(), v.privacy_mask.clone()))
            .collect();
        (cfg.face_recognition.similarity_threshold, masks)
    };
    let state_for_enroll = state.clone();
    let enroll_res = tokio::task::spawn_blocking(move || {
        let backfill_opts = should_backfill.then_some((similarity_threshold, &privacy_masks));
        state_for_enroll.face_gallery.enroll_profile(
            &state_for_enroll.db,
            &state_for_enroll.detector,
            &state_for_enroll.media_dir,
            &clean_name,
            &sample_id,
            &payload_bytes,
            event_to_assign.as_deref(),
            backfill_opts,
        )
    })
    .await;

    match enroll_res {
        Ok(Ok(outcome)) => (
            StatusCode::OK,
            Json(json!({
                "success": true,
                "name": outcome.canonical_name,
                "sample_count": outcome.sample_count,
                "embedding_dim": outcome.embedding_dim,
                "backfilled_events": outcome.backfilled_events,
                "path": outcome.saved_path.to_string_lossy()
            })),
        )
            .into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
        Err(join_err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": join_err.to_string() })),
        )
            .into_response(),
    }
}

pub async fn api_face_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Response {
    if !verify_cookie_csrf_origin(&headers, &uri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    if !caller_can_mutate(&state, &jar, &headers, true).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Admin role required" })),
        )
            .into_response();
    }
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Camera-restricted users cannot delete global face profiles" })),
        )
            .into_response();
    }
    let Some(clean_name) = ninjanvr_infer::sanitize_face_profile_name(&name) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid face profile name" })),
        )
            .into_response();
    };
    let state_for_del = state.clone();
    let clean_for_del = clean_name.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        state_for_del.face_gallery.delete_profile(
            &state_for_del.db,
            &state_for_del.detector,
            &state_for_del.media_dir,
            &clean_for_del,
        )
    })
    .await
    .unwrap_or(FaceDeletionOutcome {
        canonical_name: clean_name.clone(),
        deleted: false,
        cleared_events: 0,
    });
    Json(json!({
        "success": true,
        "name": outcome.canonical_name,
        "deleted": outcome.deleted,
        "cleared_events": outcome.cleared_events
    }))
    .into_response()
}

pub async fn api_sub_labels(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    let allowed_opt = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let state_for_subs = state.clone();
    let list = tokio::task::spawn_blocking(move || {
        state_for_subs.face_gallery.list_scoped_sub_labels(
            &state_for_subs.db,
            &state_for_subs.detector,
            &state_for_subs.media_dir,
            allowed_opt.as_deref(),
        )
    })
    .await
    .unwrap_or_default();
    Json(json!(list))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use ninjanvr_config::NinjaConfig;
    use ninjanvr_db::EventDto;
    use ninjanvr_vision::{encode_image_jpeg, encode_image_webp};

    fn make_quadrant_test_image(w: u32, h: u32) -> RgbImage {
        let mut img = RgbImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let px = match (x >= w / 2, y >= h / 2) {
                    (false, false) => Rgb([230, 30, 30]), // top-left: red
                    (true, false) => Rgb([30, 230, 30]),  // top-right: green
                    (false, true) => Rgb([30, 30, 230]),  // bottom-left: blue
                    (true, true) => Rgb([230, 230, 30]),  // bottom-right: yellow
                };
                img.put_pixel(x, y, px);
            }
        }
        img
    }

    #[test]
    fn crop_event_face_square_projects_person_and_face_box_with_boundary_clamping() {
        let src_rgb = make_quadrant_test_image(200, 200);
        let jpg_bytes = encode_image_jpeg(&src_rgb).expect("synthetic JPEG should encode");
        let webp_bytes = encode_image_webp(&src_rgb).expect("synthetic WebP should encode");

        // 1. All-zero or missing event box returns None
        let empty_ev = json!({ "box": [0.0, 0.0, 0.0, 0.0], "data": {} });
        assert!(crop_event_face_square_from_bytes(&empty_ev, &jpg_bytes).is_none());

        // 2. Person box in top-left quadrant (red) produces a 112x112 JPEG dominated by red
        let person_ev = json!({
            "box": [0.05, 0.05, 0.45, 0.45],
            "data": { "box": [0.05, 0.05, 0.45, 0.45] }
        });
        let cropped_jpg = crop_event_face_square_from_bytes(&person_ev, &jpg_bytes)
            .expect("person box crop from JPEG should succeed");
        let decoded_person = image::load_from_memory(&cropped_jpg)
            .expect("cropped JPEG should decode")
            .to_rgb8();
        assert_eq!(decoded_person.dimensions(), (112, 112));
        let center_p = decoded_person.get_pixel(56, 56);
        assert!(
            center_p[0] > 150 && center_p[1] < 80 && center_p[2] < 80,
            "expected red top-left quadrant pixel, got {center_p:?}"
        );

        // 3. Explicit data.face_box override inside bottom-right quadrant (yellow) overrides person box
        let face_override_ev = json!({
            "box": [0.05, 0.05, 0.95, 0.95],
            "data": {
                "box": [0.05, 0.05, 0.95, 0.95],
                "face_box": [0.60, 0.60, 0.90, 0.90]
            }
        });
        let cropped_from_webp = crop_event_face_square_from_bytes(&face_override_ev, &webp_bytes)
            .expect("face_box override crop from WebP should succeed");
        let decoded_face = image::load_from_memory(&cropped_from_webp)
            .expect("cropped JPEG from WebP source should decode")
            .to_rgb8();
        assert_eq!(decoded_face.dimensions(), (112, 112));
        let center_f = decoded_face.get_pixel(56, 56);
        assert!(
            center_f[0] > 150 && center_f[1] > 150 && center_f[2] < 80,
            "expected yellow bottom-right face_box pixel, got {center_f:?}"
        );

        // 4. Out-of-bounds and xywh provenance coordinates clamp safely within frame dimensions
        let clamped_ev = json!({
            "data": {
                "provenance": { "norm_bbox": [-0.5, -0.2, 2.5, 1.8] }
            }
        });
        let bbox = norm_box_to_pixel_bbox(&clamped_ev, [-0.5, -0.2, 2.5, 1.8], 200, 100)
            .expect("clamped bbox should resolve");
        assert_eq!(bbox.x_min, 0);
        assert_eq!(bbox.y_min, 0);
        assert_eq!(bbox.x_max, 200);
        assert_eq!(bbox.y_max, 100);
        assert!(norm_box_to_pixel_bbox(&clamped_ev, [0.1, 0.1, 0.5, 0.5], 0, 100).is_none());
    }

    #[test]
    fn clips_mirror_lifecycle_and_path_traversal_rejection() {
        let nonce: u64 = rand::random();
        let tmp_dir = std::env::temp_dir().join(format!("ninjanvr_face_mirror_test_{nonce:016x}"));
        let media_dir = tmp_dir.join("media");
        std::fs::create_dir_all(&media_dir).unwrap();

        let sample_src = tmp_dir.join("ev_100.jpg");
        std::fs::write(&sample_src, b"FAKE_JPEG_FACE_BYTES").unwrap();

        // 1. Valid profile name mirrors into media_dir/clips/faces/Alice/ev_100.jpg
        mirror_face_sample_to_clips(&media_dir, "Alice", &sample_src);
        let mirrored_file = media_dir.join("clips/faces/Alice/ev_100.jpg");
        assert!(mirrored_file.is_file());
        assert_eq!(
            std::fs::read(&mirrored_file).unwrap(),
            b"FAKE_JPEG_FACE_BYTES"
        );

        // 2. Traversal profile names ("../escape", "unknown", "") are rejected without creating files
        let outside_sentinel = tmp_dir.join("escape");
        mirror_face_sample_to_clips(&media_dir, "../escape", &sample_src);
        mirror_face_sample_to_clips(&media_dir, "unknown", &sample_src);
        assert!(!outside_sentinel.exists());
        assert!(!media_dir.join("clips/faces/unknown").exists());

        // 3. remove_face_profile_from_clips_mirror removes Alice case-insensitively and ignores traversal
        remove_face_profile_from_clips_mirror(&media_dir, "../escape");
        assert!(mirrored_file.is_file(), "Alice mirror must remain intact");

        remove_face_profile_from_clips_mirror(&media_dir, "alice");
        assert!(
            !media_dir.join("clips/faces/Alice").exists(),
            "case-insensitive profile removal must delete clips/faces/Alice"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn face_identity_catalog_enroll_assign_and_delete_clears_event_sub_labels() {
        let nonce: u64 = rand::random();
        let tmp_dir = std::env::temp_dir().join(format!("ninjanvr_face_catalog_test_{nonce:016x}"));
        let media_dir = tmp_dir.join("media");
        std::fs::create_dir_all(&media_dir).unwrap();

        let db_path = tmp_dir.join("ninjanvr.db");
        let db = NinjaDatabase::open(&db_path).expect("temp database should open");
        let cfg_path = tmp_dir.join("config.yml");
        std::fs::write(
            &cfg_path,
            format!(
                "version: 0.18-0\ndatabase:\n  path: {}\ncameras: {{}}\n",
                db_path.display()
            ),
        )
        .unwrap();
        let cfg = NinjaConfig::load(&cfg_path).expect("config should load");
        let detector = AiDetectorEngine::new(&cfg);
        let catalog = FaceIdentityCatalog::new();

        // Seed two person events on front_door and backyard
        for (id, cam) in [("ev_face_1", "front_door"), ("ev_face_2", "backyard")] {
            db.upsert_event(&EventDto {
                id: id.to_string(),
                camera: cam.to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 1_700_000_000.0,
                end_time: Some(1_700_000_005.0),
                has_clip: false,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.93),
                false_positive: Some(false),
                box_coords: Some(json!([0.1, 0.1, 0.5, 0.8])),
                data: json!({ "box": [0.1, 0.1, 0.5, 0.8] }),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.93),
                stationary: false,
            })
            .unwrap();
        }

        // 1. Enroll profile "Alice" linked to ev_face_1
        let img = make_quadrant_test_image(112, 112);
        let sample_jpg = encode_image_jpeg(&img).unwrap();
        let enrolled = catalog
            .enroll_profile(
                &db,
                &detector,
                &media_dir,
                "Alice",
                "ev_face_1",
                &sample_jpg,
                Some("ev_face_1"),
                None,
            )
            .expect("enroll_profile should succeed");
        assert_eq!(enrolled.canonical_name, "Alice");
        assert_eq!(enrolled.sample_count, 1);
        assert!(enrolled.embedding_dim > 0);
        assert!(enrolled.saved_path.is_file());
        assert!(
            media_dir.join("clips/faces/Alice").is_dir(),
            "enroll_profile must mirror sample into clips/faces/Alice"
        );

        let ev1 = db.get_event("ev_face_1").unwrap().unwrap();
        assert_eq!(ev1["sub_label"].as_str(), Some("Alice"));

        // 2. assign_event_sub_label resolves case-insensitive "alice" -> canonical "Alice" on ev_face_2
        let assigned = catalog.assign_event_sub_label(
            &db,
            &detector,
            &media_dir,
            "ev_face_2",
            Some("alice"),
            Some(0.88),
            None,
        );
        assert_eq!(assigned.updated, 1);
        assert_eq!(assigned.canonical_sub_label.as_deref(), Some("Alice"));
        let ev2 = db.get_event("ev_face_2").unwrap().unwrap();
        assert_eq!(ev2["sub_label"].as_str(), Some("Alice"));

        // 3. Scoped profile & sub_label queries respect camera permissions
        let fd_scope = vec!["front_door".to_string()];
        let scoped_profiles =
            catalog.list_scoped_profiles(&db, &detector, &media_dir, Some(&fd_scope));
        assert_eq!(scoped_profiles.len(), 1);
        assert_eq!(scoped_profiles[0].name, "Alice");
        let scoped_subs =
            catalog.list_scoped_sub_labels(&db, &detector, &media_dir, Some(&fd_scope));
        assert_eq!(scoped_subs, vec!["Alice".to_string()]);

        // 4. Deleting profile "Alice" removes faces dir, clips mirror, and clears sub_labels on both events
        let deleted = catalog.delete_profile(&db, &detector, &media_dir, "Alice");
        assert!(deleted.deleted);
        assert_eq!(deleted.cleared_events, 2);
        assert!(!media_dir.join("clips/faces/Alice").exists());
        assert!(
            db.get_event("ev_face_1").unwrap().unwrap()["sub_label"].is_null(),
            "ev_face_1 sub_label must be cleared after profile deletion"
        );
        assert!(
            db.get_event("ev_face_2").unwrap().unwrap()["sub_label"].is_null(),
            "ev_face_2 sub_label must be cleared after profile deletion"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
