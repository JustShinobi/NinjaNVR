#![forbid(unsafe_code)]

use std::collections::HashMap;

use ninjanvr_config::{CameraConfig, FaceRecognitionConfig, NinjaConfig, ProfileOverrideConfig};
use ninjanvr_db::{EventDto, ReviewItemDto};
use ninjanvr_infer::LprConsensusResult;
use ninjanvr_media::{YuvFrame, privacy_mask_is_configured};
use ninjanvr_track::{
    TrackedAttribute, TrackedObject, TriggerProvenance, compute_attribute_quality,
};
use ninjanvr_vision::{
    BoundingBox, classify_dominant_color_tag, compute_crop_dhash64, is_monochrome_ir,
};
use serde_json::json;

pub const MAX_TRACK_SNAPSHOTS: usize = 512;
pub const MAX_REVIEW_DETECTIONS: usize = 256;
pub const MAX_REVIEW_DURATION_SECS: f64 = 300.0;
pub const ALERT_BACKUP_CHECKPOINT_SECS: f64 = 60.0;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackPersistSnapshot {
    pub best_score: f32,
    pub best_attribute_score: f32,
    pub sub_label: Option<String>,
    pub sub_label_score: Option<f32>,
    pub detected_attributes: Vec<TrackedAttribute>,
    pub zones: Vec<String>,
    pub stationary: bool,
    pub dhash64: Option<u64>,
    pub suppressed_fp: bool,
    pub matched_fp_sig_id: Option<i64>,
    pub ai_verify_triggered: bool,
}

#[derive(Clone, Debug)]
pub struct ActiveReviewState {
    pub id: String,
    pub start_time: f64,
    pub last_activity_time: f64,
    pub last_backup_time: f64,
    pub severity: String,
    pub thumb_path: String,
    pub thumb_time: f64,
    pub best_score: f32,
    pub detections: Vec<String>,
    pub objects: Vec<String>,
    pub sub_labels: Vec<String>,
    pub sub_label_score: Option<f32>,
    pub detected_attributes: Vec<TrackedAttribute>,
    pub zones: Vec<String>,
    pub provenance: Option<TriggerProvenance>,
    pub is_new: bool,
    pub dirty: bool,
    pub prev_dto: Option<ReviewItemDto>,
}

impl ActiveReviewState {
    pub fn to_dto(&self, camera: &str, end_time: Option<f64>) -> ReviewItemDto {
        ReviewItemDto {
            id: self.id.clone(),
            camera: camera.to_string(),
            start_time: self.start_time,
            end_time,
            severity: self.severity.clone(),
            thumb_path: self.thumb_path.clone(),
            data: json!({
                "detections": self.detections,
                "objects": self.objects,
                "verified_objects": [],
                "sub_labels": self.sub_labels,
                "sub_label_score": self.sub_label_score,
                "detected_attributes": self.detected_attributes,
                "zones": self.zones,
                "audio": [],
                "thumb_time": self.thumb_time,
                "provenance": self.provenance,
                "metadata": null
            }),
            has_been_reviewed: false,
        }
    }

    /// Absorbs an active track's metadata into the review segment and returns `true` when
    /// the review thumbnail should be written/replaced.
    pub fn absorb_track(
        &mut self,
        trk: &TrackedObject,
        enriched: &EnrichedTrackMetadata,
        is_alert: bool,
        now: f64,
    ) -> bool {
        let already_in_rev = self.detections.contains(&trk.id);
        if !trk.stationary || !already_in_rev {
            self.last_activity_time = now;
        }
        let promoted_to_alert = is_alert && self.severity != "alert";
        if promoted_to_alert {
            self.severity = "alert".to_string();
            self.dirty = true;
        }
        if !already_in_rev && self.detections.len() < MAX_REVIEW_DETECTIONS {
            self.detections.push(trk.id.clone());
            self.dirty = true;
        }
        if !self.objects.contains(&trk.label) && self.objects.len() < MAX_REVIEW_DETECTIONS {
            self.objects.push(trk.label.clone());
            self.dirty = true;
        }
        if let Some(ref sl) = enriched.effective_sub_label
            && !self.sub_labels.contains(sl)
            && self.sub_labels.len() < MAX_REVIEW_DETECTIONS
        {
            self.sub_labels.push(sl.clone());
            self.dirty = true;
        }
        if let Some(sc) = enriched.effective_sub_label_score
            && self
                .sub_label_score
                .map(|prev_sc| sc > prev_sc)
                .unwrap_or(true)
        {
            self.sub_label_score = Some(sc);
            self.dirty = true;
        }
        for attr in &enriched.merged_attributes {
            if !self
                .detected_attributes
                .iter()
                .any(|a| a.label == attr.label)
                && self.detected_attributes.len() < MAX_REVIEW_DETECTIONS
            {
                self.detected_attributes.push(attr.clone());
                self.dirty = true;
            }
        }
        for z in &trk.entered_zones {
            if !self.zones.contains(z) && self.zones.len() < MAX_REVIEW_DETECTIONS {
                self.zones.push(z.clone());
                self.dirty = true;
            }
        }
        let should_update_thumb =
            self.is_new || trk.score > self.best_score + 0.05 || promoted_to_alert;
        if should_update_thumb {
            self.best_score = trk.score.max(self.best_score);
            self.thumb_time = now;
            self.provenance = Some(trk.provenance.clone());
        }
        should_update_thumb
    }
}

/// Evicts stale entries from `track_snapshots` when capacity reaches `MAX_TRACK_SNAPSHOTS`,
/// preserving snapshots for currently active or ending tracks.
pub fn bound_track_snapshots_capacity(
    track_snapshots: &mut HashMap<String, TrackPersistSnapshot>,
    active_tracks: &[TrackedObject],
    ended_tracks: &[TrackedObject],
) {
    if track_snapshots.len() >= MAX_TRACK_SNAPSHOTS {
        track_snapshots.retain(|k, _| {
            active_tracks.iter().any(|t| &t.id == k) || ended_tracks.iter().any(|t| &t.id == k)
        });
        while track_snapshots.len() >= MAX_TRACK_SNAPSHOTS {
            let Some(evict_id) = track_snapshots
                .keys()
                .find(|k| !active_tracks.iter().any(|t| &t.id == *k))
                .or_else(|| track_snapshots.keys().next())
                .cloned()
            else {
                break;
            };
            track_snapshots.remove(&evict_id);
        }
    }
}

/// Merges a track's current-frame attributes into `prev_attrs`, deduplicating by attribute `label`
/// and keeping the observation with higher confidence score or larger pixel area.
pub fn merge_tracked_attributes(
    prev_attrs: Option<&[TrackedAttribute]>,
    current_attrs: &[TrackedAttribute],
) -> Vec<TrackedAttribute> {
    let mut merged: Vec<TrackedAttribute> = prev_attrs.map(|s| s.to_vec()).unwrap_or_default();
    for attr in current_attrs {
        if let Some(existing) = merged.iter_mut().find(|a| a.label == attr.label) {
            if attr.score >= existing.score || attr.area_px >= existing.area_px {
                *existing = attr.clone();
            }
        } else {
            merged.push(attr.clone());
        }
    }
    merged
}

/// Resolves the candidate face crop bounding box when face recognition should run for an active
/// `"person"` track whose sub-label is still unset or `"unknown"`.
pub fn resolve_face_recognition_candidate_bbox(
    trk: &TrackedObject,
    prev: Option<&TrackPersistSnapshot>,
    lpr_consensus: Option<&LprConsensusResult>,
    face_cfg: &FaceRecognitionConfig,
    frame_width: u32,
    frame_height: u32,
) -> Option<BoundingBox> {
    if !face_cfg.enabled || trk.label != "person" {
        return None;
    }
    let initial_sub_label = lpr_consensus
        .and_then(|c| c.sub_label.as_deref())
        .or(trk.inferred_sub_label.as_deref())
        .or_else(|| prev.and_then(|p| p.sub_label.as_deref()));
    if initial_sub_label.is_some() && initial_sub_label != Some("unknown") {
        return None;
    }
    let merged_attrs = merge_tracked_attributes(
        prev.map(|p| p.detected_attributes.as_slice()),
        &trk.attributes,
    );
    let face_attr_bbox = merged_attrs
        .iter()
        .find(|a| a.label.eq_ignore_ascii_case("face"))
        .map(|a| a.bbox);
    ninjanvr_infer::resolve_face_crop_bbox(
        trk.bbox,
        face_attr_bbox,
        frame_width,
        frame_height,
        face_cfg.min_face_area,
    )
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnrichedTrackMetadata {
    pub is_new: bool,
    pub newly_entered_zones: Vec<String>,
    pub prev_best_score: f32,
    pub best_score: f32,
    pub prev_best_attr_quality: f32,
    pub new_attr_quality: f32,
    pub best_attr_score: f32,
    pub is_better_score: bool,
    pub is_better_attr: bool,
    pub state_changed: bool,
    pub attrs_changed: bool,
    pub recognized_license_plate: Option<String>,
    pub effective_sub_label: Option<String>,
    pub effective_sub_label_score: Option<f32>,
    pub merged_attributes: Vec<TrackedAttribute>,
    pub norm_box: [f64; 4],
    pub norm_reg: [f64; 4],
    pub crop_dhash64: u64,
    pub dominant_color_tag: String,
    pub attribute_tags: Vec<String>,
    pub face_box: Option<[f64; 4]>,
    pub license_plate_box: Option<[f64; 4]>,
    pub area: u32,
    pub ratio: f32,
    pub severity: &'static str,
}

impl EnrichedTrackMetadata {
    /// Recomputes track severity when `suppressed_fp` changes after initial enrichment.
    pub fn resolve_severity(
        &self,
        trk: &TrackedObject,
        cam_cfg: &CameraConfig,
        active_profile: &str,
        camera_suppressed_in_profile: bool,
        alerts_enabled: bool,
        suppressed_fp: bool,
    ) -> &'static str {
        if suppressed_fp || !alerts_enabled {
            return "detection";
        }
        let mut trk_for_sev = trk.clone();
        if trk_for_sev.inferred_sub_label.is_none() {
            trk_for_sev.inferred_sub_label = self.effective_sub_label.clone();
        }
        compute_track_severity_for_profile(
            cam_cfg,
            &trk_for_sev,
            active_profile,
            camera_suppressed_in_profile,
        )
    }
}

pub struct TrackEnrichmentInput<'a> {
    pub trk: &'a TrackedObject,
    pub prev: Option<&'a TrackPersistSnapshot>,
    pub lpr_consensus: Option<&'a LprConsensusResult>,
    pub face_match: Option<(String, f32)>,
    pub frame: &'a YuvFrame,
    pub cam_cfg: &'a CameraConfig,
    pub active_profile: &'a str,
    pub camera_suppressed_in_profile: bool,
    pub alerts_enabled: bool,
    pub suppressed_fp: bool,
    pub is_ended: bool,
}

/// Enriches a tracked object (active or ended) with deduplicated attributes, LPR/face sub-labels,
/// perceptual dHash64, dominant color classification, normalized boxes, and profile-aware severity.
pub fn enrich_tracked_object(input: TrackEnrichmentInput<'_>) -> EnrichedTrackMetadata {
    let trk = input.trk;
    let prev = input.prev;
    let frame = input.frame;

    let is_new = prev.is_none();
    let prev_zones: &[String] = prev.map(|p| p.zones.as_slice()).unwrap_or(&[]);
    let newly_entered_zones: Vec<String> = trk
        .entered_zones
        .iter()
        .filter(|z| !prev_zones.contains(z))
        .cloned()
        .collect();

    let prev_best_score = prev.map(|p| p.best_score).unwrap_or(0.0);
    let best_score = if input.is_ended {
        prev.map(|s| s.best_score.max(trk.top_score))
            .unwrap_or(trk.top_score)
    } else {
        prev_best_score.max(trk.score)
    };

    let prev_best_attr_quality = prev.map(|p| p.best_attribute_score).unwrap_or(0.0);
    let attr_q_from_boxes = trk
        .attributes
        .iter()
        .filter(|a| {
            a.label.eq_ignore_ascii_case("face") || a.label.eq_ignore_ascii_case("license_plate")
        })
        .map(|a| compute_attribute_quality(a.score, a.area_px, frame.width, frame.height))
        .fold(0.0f32, f32::max);
    let new_attr_quality = if attr_q_from_boxes > 0.0 {
        attr_q_from_boxes.max(trk.best_attribute_score)
    } else if trk.best_attribute_score > prev_best_attr_quality {
        trk.best_attribute_score
    } else {
        0.0
    };
    let best_attr_score = prev_best_attr_quality.max(new_attr_quality);

    let is_better_score = prev
        .map(|p| trk.score > p.best_score + 0.05)
        .unwrap_or(true);
    let is_better_attr = (prev_best_attr_quality <= 0.0 && new_attr_quality > 0.0)
        || new_attr_quality > prev_best_attr_quality + 0.02;
    let state_changed = prev
        .map(|p| p.zones != trk.entered_zones || p.stationary != trk.stationary)
        .unwrap_or(false);

    let recognized_license_plate = input.lpr_consensus.map(|c| c.plate.clone());
    let mut effective_sub_label = input
        .lpr_consensus
        .and_then(|c| c.sub_label.clone())
        .or_else(|| trk.inferred_sub_label.clone())
        .or_else(|| prev.and_then(|p| p.sub_label.clone()));
    let mut effective_sub_label_score: Option<f32> = prev.and_then(|p| p.sub_label_score);

    let merged_attributes = merge_tracked_attributes(
        prev.map(|p| p.detected_attributes.as_slice()),
        &trk.attributes,
    );

    if !input.is_ended
        && trk.label == "person"
        && (effective_sub_label.is_none() || effective_sub_label.as_deref() == Some("unknown"))
        && let Some((matched_sub_label, matched_score)) = input.face_match
    {
        effective_sub_label = Some(matched_sub_label);
        effective_sub_label_score = Some(matched_score);
    }

    let attrs_changed = prev
        .map(|p| {
            p.sub_label != effective_sub_label
                || p.sub_label_score != effective_sub_label_score
                || p.detected_attributes != merged_attributes
        })
        .unwrap_or(!merged_attributes.is_empty() || effective_sub_label.is_some());

    let norm_box = trk.bbox.to_normalized(frame.width, frame.height);
    let norm_reg = trk.region.to_normalized(frame.width, frame.height);
    let crop_dhash64 = if input.is_ended {
        prev.and_then(|s| s.dhash64).unwrap_or_else(|| {
            compute_crop_dhash64(&frame.data, frame.width, frame.height, trk.bbox)
        })
    } else {
        compute_crop_dhash64(&frame.data, frame.width, frame.height, trk.bbox)
    };

    let dominant_color_tag =
        classify_dominant_color_tag(&frame.data, frame.width, frame.height, trk.bbox).to_string();
    let mut attribute_tags = vec![dominant_color_tag.clone()];
    for a in &merged_attributes {
        if !attribute_tags.contains(&a.label) {
            attribute_tags.push(a.label.clone());
        }
    }
    if let Some(ref plate_str) = recognized_license_plate
        && !attribute_tags.contains(plate_str)
    {
        attribute_tags.push(plate_str.clone());
    }

    let face_box = merged_attributes
        .iter()
        .find(|a| a.label.eq_ignore_ascii_case("face"))
        .map(|a| a.norm_bbox);
    let license_plate_box = merged_attributes
        .iter()
        .find(|a| a.label.eq_ignore_ascii_case("license_plate"))
        .map(|a| a.norm_bbox)
        .or_else(|| {
            if trk.label.eq_ignore_ascii_case("license_plate") {
                Some(norm_box)
            } else {
                None
            }
        });

    let mut trk_for_sev = trk.clone();
    if trk_for_sev.inferred_sub_label.is_none() {
        trk_for_sev.inferred_sub_label = effective_sub_label.clone();
    }
    let severity = if input.suppressed_fp || !input.alerts_enabled {
        "detection"
    } else {
        compute_track_severity_for_profile(
            input.cam_cfg,
            &trk_for_sev,
            input.active_profile,
            input.camera_suppressed_in_profile,
        )
    };

    let area = trk.bbox.area();
    let ratio = (trk.bbox.width() as f32) / (trk.bbox.height().max(1) as f32);

    EnrichedTrackMetadata {
        is_new,
        newly_entered_zones,
        prev_best_score,
        best_score,
        prev_best_attr_quality,
        new_attr_quality,
        best_attr_score,
        is_better_score,
        is_better_attr,
        state_changed,
        attrs_changed,
        recognized_license_plate,
        effective_sub_label,
        effective_sub_label_score,
        merged_attributes,
        norm_box,
        norm_reg,
        crop_dhash64,
        dominant_color_tag,
        attribute_tags,
        face_box,
        license_plate_box,
        area,
        ratio,
        severity,
    }
}

pub struct BuildTrackEventDtoParams<'a> {
    pub camera: &'a str,
    pub trk: &'a TrackedObject,
    pub enriched: &'a EnrichedTrackMetadata,
    pub record_active: bool,
    pub snapshots_active: bool,
    pub suppressed_fp: bool,
    pub matched_fp_sig_id: Option<i64>,
    pub severity: &'a str,
    pub thumbnail: Option<String>,
    pub end_time: Option<f64>,
    pub night_mode: Option<bool>,
}

/// Builds an `EventDto` for either an active track (`end_time = None`) or a finalized ended track
/// (`end_time = Some(last_seen_time)`).
pub fn build_track_event_dto(params: BuildTrackEventDtoParams<'_>) -> EventDto {
    let trk = params.trk;
    let enriched = params.enriched;
    let mut data_obj = json!({
        "box": enriched.norm_box,
        "region": enriched.norm_reg,
        "dhash64": enriched.crop_dhash64,
        "suppressed_by_fp_signature": params.matched_fp_sig_id,
        "score": trk.score,
        "top_score": enriched.best_score,
        "sub_label": enriched.effective_sub_label,
        "sub_label_score": enriched.effective_sub_label_score,
        "attributes": enriched.attribute_tags,
        "detected_attributes": enriched.merged_attributes,
        "face_box": enriched.face_box,
        "license_plate_box": enriched.license_plate_box,
        "recognized_license_plate": enriched.recognized_license_plate,
        "type": "object",
        "max_severity": params.severity,
        "provenance": trk.provenance
    });
    if let Some(nm) = params.night_mode
        && let Some(map) = data_obj.as_object_mut()
    {
        map.insert("night_mode".to_string(), json!(nm));
    }

    EventDto {
        id: trk.id.clone(),
        camera: params.camera.to_string(),
        label: trk.label.clone(),
        zones: json!(trk.entered_zones),
        start_time: trk.start_time,
        end_time: params.end_time,
        has_clip: params.record_active,
        has_snapshot: params.snapshots_active,
        plus_id: None,
        retain_indefinitely: false,
        sub_label: enriched.effective_sub_label.clone(),
        top_score: Some(enriched.best_score as f64),
        false_positive: Some(params.suppressed_fp),
        box_coords: Some(json!(enriched.norm_box)),
        data: data_obj,
        thumbnail: params.thumbnail,
        area: Some(enriched.area),
        ratio: Some(enriched.ratio),
        score: Some(trk.score),
        stationary: trk.stationary,
    }
}

/// Updates `rev.severity` based on whether any remaining non-false-positive detection in the
/// review segment has `"alert"` severity.
pub fn recompute_active_review_severity(rev: &mut ActiveReviewState, has_remaining_alert: bool) {
    rev.severity = if has_remaining_alert {
        "alert".to_string()
    } else {
        "detection".to_string()
    };
}

#[derive(Clone, Debug)]
pub enum ReviewFalsePositiveTransition {
    /// The track was not part of any active review segment.
    Unchanged,
    /// The false-positive track was removed from `detections`, other tracks remain in the review,
    /// and the review's severity was updated (`"alert"` or demoted to `"detection"`).
    RetainedWithUpdatedSeverity { new_severity: String },
    /// Removing the false-positive track left the review with zero detections, so `active_review`
    /// was cleared (`None`). If `persisted_in_db` is `true`, the caller should delete `review_id`
    /// from SQLite and broadcast the `"end"` event.
    DeleteEmptyReview {
        review_id: String,
        persisted_in_db: bool,
        before_dto: Box<ReviewItemDto>,
        closed_dto: Box<ReviewItemDto>,
    },
}

/// Removes a false-positive `track_id` from `active_review` (if present) and performs the pure
/// state transition:
/// - If `detections` becomes empty, clears `active_review` to `None` and returns
///   `ReviewFalsePositiveTransition::DeleteEmptyReview`.
/// - If other detections remain, recomputes the review's severity using `has_remaining_alert_fn`
///   and returns `ReviewFalsePositiveTransition::RetainedWithUpdatedSeverity`.
pub fn prune_false_positive_from_active_review<F>(
    active_review: &mut Option<ActiveReviewState>,
    track_id: &str,
    camera: &str,
    now: f64,
    has_remaining_alert_fn: F,
) -> ReviewFalsePositiveTransition
where
    F: FnOnce(&[String]) -> bool,
{
    let Some(rev) = active_review.as_mut() else {
        return ReviewFalsePositiveTransition::Unchanged;
    };
    let Some(pos) = rev.detections.iter().position(|d| d == track_id) else {
        return ReviewFalsePositiveTransition::Unchanged;
    };

    rev.detections.remove(pos);
    rev.dirty = true;

    if rev.detections.is_empty() {
        let removed_rev = active_review.take().expect("active_review was Some");
        let closed_dto = removed_rev.to_dto(camera, Some(now));
        let before_dto = removed_rev
            .prev_dto
            .clone()
            .unwrap_or_else(|| closed_dto.clone());
        ReviewFalsePositiveTransition::DeleteEmptyReview {
            review_id: removed_rev.id,
            persisted_in_db: !removed_rev.is_new,
            before_dto: Box::new(before_dto),
            closed_dto: Box::new(closed_dto),
        }
    } else {
        let has_remaining_alert = has_remaining_alert_fn(&rev.detections);
        recompute_active_review_severity(rev, has_remaining_alert);
        ReviewFalsePositiveTransition::RetainedWithUpdatedSeverity {
            new_severity: rev.severity.clone(),
        }
    }
}

/// Determines whether an active non-suppressed track should open or update a review segment.
/// Enforces detection zone filtering and prevents already-tracked stationary objects from
/// endlessly re-opening new review segments after their initial review expires.
pub fn should_include_track_in_review(
    cam_cfg: &CameraConfig,
    trk: &TrackedObject,
    track_severity: &str,
    has_active_review: bool,
    is_new_track: bool,
    newly_entered_zones: &[String],
) -> bool {
    let is_alert = track_severity == "alert";
    let detection_zone_ok = cam_cfg.review.detections.required_zones.is_empty()
        || trk
            .entered_zones
            .iter()
            .any(|z| cam_cfg.review.detections.required_zones.contains(z));
    if !is_alert && (!cam_cfg.review.detections.enabled || !detection_zone_ok) {
        return false;
    }
    if !has_active_review && trk.stationary && !is_new_track && newly_entered_zones.is_empty() {
        return false;
    }
    true
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReviewCutoffCheckpointDecision {
    pub cutoff_secs: f64,
    pub has_non_stationary_active: bool,
    pub is_expired: bool,
    pub should_flush: bool,
    pub end_time: Option<f64>,
    pub ws_msg_type: &'static str,
    pub reached_60s_checkpoint: bool,
    pub should_advance_backup_time: bool,
    pub should_spawn_alert_backup: bool,
}

/// Evaluates whether an `ActiveReviewState` has reached its stationary/inactivity cutoff or
/// maximum duration (`300s`), whether it should be flushed to SQLite/WebSocket, and whether a
/// 60-second (or final) alert clip backup checkpoint should be triggered.
pub fn evaluate_review_cutoff_and_checkpoint(
    rev: &ActiveReviewState,
    cam_cfg: &CameraConfig,
    active_tracks: &[TrackedObject],
    now: f64,
) -> ReviewCutoffCheckpointDecision {
    let cutoff_secs = if rev.severity == "alert" {
        cam_cfg.review.alerts.cutoff_time.max(15) as f64
    } else {
        cam_cfg.review.detections.cutoff_time.max(15) as f64
    };
    let has_non_stationary_active = active_tracks.iter().any(|t| !t.stationary);
    let is_expired = (!has_non_stationary_active && (now - rev.last_activity_time) >= cutoff_secs)
        || (now - rev.start_time) >= MAX_REVIEW_DURATION_SECS;
    let should_flush = rev.dirty || is_expired;
    let end_time = if is_expired {
        Some(rev.last_activity_time)
    } else {
        None
    };
    let ws_msg_type = if is_expired {
        "end"
    } else if rev.is_new {
        "new"
    } else {
        "update"
    };
    let reached_60s_checkpoint = (now - rev.last_backup_time) >= ALERT_BACKUP_CHECKPOINT_SECS;
    let should_advance_backup_time = should_flush && (is_expired || reached_60s_checkpoint);
    let bak_cfg = &cam_cfg.record.alert_backup;
    let should_spawn_alert_backup = should_advance_backup_time
        && bak_cfg.enabled
        && !privacy_mask_is_configured(Some(&cam_cfg.privacy_mask))
        && bak_cfg.severities.iter().any(|s| s == &rev.severity)
        && bak_cfg.target_dir.is_some();

    ReviewCutoffCheckpointDecision {
        cutoff_secs,
        has_non_stationary_active,
        is_expired,
        should_flush,
        end_time,
        ws_msg_type,
        reached_60s_checkpoint,
        should_advance_backup_time,
        should_spawn_alert_backup,
    }
}

pub fn lookup_profile_override<'a>(
    cfg: &'a NinjaConfig,
    active_profile: &str,
) -> Option<&'a ProfileOverrideConfig> {
    cfg.profiles.get(active_profile).or_else(|| {
        cfg.profiles
            .iter()
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(active_profile.trim()))
            .map(|(_, v)| v)
    })
}

pub fn is_camera_alert_suppressed_in_profile(
    cfg: &NinjaConfig,
    active_profile: &str,
    camera: &str,
) -> bool {
    let prof_cfg = lookup_profile_override(cfg, active_profile);
    prof_cfg
        .map(|p| {
            p.disabled_alerts_cameras
                .iter()
                .any(|c| c == camera || c == "*")
        })
        .unwrap_or(false)
}

pub fn compute_track_severity_for_profile(
    cam_cfg: &CameraConfig,
    trk: &TrackedObject,
    active_profile: &str,
    camera_suppressed_in_profile: bool,
) -> &'static str {
    if active_profile == "disarmed"
        || camera_suppressed_in_profile
        || (active_profile == "home" && cam_cfg.indoor)
    {
        return "detection";
    }
    compute_track_severity(cam_cfg, trk)
}

pub fn compute_track_severity(cam_cfg: &CameraConfig, trk: &TrackedObject) -> &'static str {
    if !cam_cfg.review.alerts.enabled
        || !cam_cfg.review.alerts.labels.contains(&trk.label)
        || (!cam_cfg.review.alerts.required_zones.is_empty()
            && !trk
                .entered_zones
                .iter()
                .any(|z| cam_cfg.review.alerts.required_zones.contains(z)))
    {
        return "detection";
    }

    if !cam_cfg.review.alerts.exclude_sub_labels.is_empty()
        && let Some(ref sub) = trk.inferred_sub_label
        && cam_cfg
            .review
            .alerts
            .exclude_sub_labels
            .iter()
            .any(|ex| ex.eq_ignore_ascii_case(sub))
    {
        return "detection";
    }

    if !cam_cfg.review.alerts.sub_labels.is_empty() {
        let matched_allowed = trk.inferred_sub_label.as_ref().is_some_and(|sub| {
            cam_cfg
                .review
                .alerts
                .sub_labels
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(sub))
        });
        if !matched_allowed {
            return "detection";
        }
    }

    "alert"
}

/// Detects whether a YUV420p frame is in monochrome IR night mode based on U/V chroma plane variance.
pub fn is_frame_night_ir(yuv_data: &[u8], width: u32, height: u32) -> bool {
    is_monochrome_ir(yuv_data, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn sample_track() -> TrackedObject {
        TrackedObject {
            id: "1730500000.000000-trk01".to_string(),
            label: "person".to_string(),
            bbox: BoundingBox::new(16, 16, 112, 160),
            region: BoundingBox::new(0, 0, 320, 180),
            score: 0.88,
            top_score: 0.91,
            start_time: 1000.0,
            last_seen_time: 1005.0,
            frames_tracked: 6,
            disappeared_frames: 0,
            motionless_count: 0,
            stationary: false,
            zones: Vec::new(),
            entered_zones: Vec::new(),
            crossed_lines: Vec::new(),
            prev_bottom_center: (64.0, 160.0),
            has_snapshot: true,
            vx: 0.0,
            vy: 0.0,
            provenance: TriggerProvenance {
                trigger_ts: 1000.0,
                bbox: [16, 16, 112, 160],
                norm_bbox: [0.05, 0.088, 0.35, 0.888],
                area_px: 13824,
                area_pct: 24.0,
                aspect_ratio: 0.666,
                score: 0.88,
                top_score: 0.91,
                min_score_cfg: 0.5,
                threshold_cfg: 0.7,
                zones: Vec::new(),
                frames_tracked: 6,
                stationary: false,
            },
            attributes: Vec::new(),
            best_attribute_score: 0.0,
            inferred_sub_label: None,
        }
    }

    fn synthetic_yuv_frame(width: u32, height: u32, y_val: u8, u_val: u8, v_val: u8) -> YuvFrame {
        let y_len = (width * height) as usize;
        let uv_len = y_len / 4;
        let mut data = vec![y_val; y_len + uv_len * 2];
        data[y_len..y_len + uv_len].fill(u_val);
        data[y_len + uv_len..].fill(v_val);
        YuvFrame {
            camera: Arc::from("front_door"),
            timestamp: 1000.0,
            width,
            height,
            data: Arc::new(data),
        }
    }

    fn sample_active_review(
        detections: Vec<&str>,
        severity: &str,
        is_new: bool,
    ) -> ActiveReviewState {
        ActiveReviewState {
            id: "1000.000000-rev01".to_string(),
            start_time: 1000.0,
            last_activity_time: 1002.0,
            last_backup_time: 1000.0,
            severity: severity.to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-front_door-rev01.webp".to_string(),
            thumb_time: 1000.0,
            best_score: 0.90,
            detections: detections.into_iter().map(String::from).collect(),
            objects: vec!["person".to_string()],
            sub_labels: Vec::new(),
            sub_label_score: None,
            detected_attributes: Vec::new(),
            zones: Vec::new(),
            provenance: None,
            is_new,
            dirty: false,
            prev_dto: None,
        }
    }

    #[test]
    fn enrich_tracked_object_deduplicates_attributes_resolves_lpr_and_face_and_builds_event_dto() {
        let w = 320u32;
        let h = 180u32;
        // Saturated red frame (Y=90, U=90, V=240)
        let frame = synthetic_yuv_frame(w, h, 90, 90, 240);
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.review.alerts.enabled = true;
        cam_cfg.review.alerts.labels = vec!["person".to_string(), "car".to_string()];

        // 1. Attribute deduplication keeps higher score/area and face match populates person sub_label
        let prev = TrackPersistSnapshot {
            best_score: 0.85,
            best_attribute_score: 0.40,
            sub_label: Some("unknown".to_string()),
            sub_label_score: Some(0.51),
            detected_attributes: vec![
                TrackedAttribute {
                    label: "face".to_string(),
                    score: 0.72,
                    bbox: BoundingBox::new(30, 20, 60, 50),
                    norm_bbox: [0.1, 0.1, 0.2, 0.3],
                    area_px: 900,
                },
                TrackedAttribute {
                    label: "amazon".to_string(),
                    score: 0.88,
                    bbox: BoundingBox::new(40, 60, 90, 110),
                    norm_bbox: [0.12, 0.33, 0.28, 0.61],
                    area_px: 2500,
                },
            ],
            zones: vec!["porch".to_string()],
            stationary: false,
            dhash64: Some(0x1122_3344_5566_7788),
            suppressed_fp: false,
            matched_fp_sig_id: None,
            ai_verify_triggered: false,
        };

        let mut trk = sample_track();
        trk.score = 0.92;
        trk.entered_zones = vec!["porch".to_string(), "front_steps".to_string()];
        trk.attributes = vec![
            // Higher score face replaces previous face attribute
            TrackedAttribute {
                label: "face".to_string(),
                score: 0.94,
                bbox: BoundingBox::new(32, 22, 68, 58),
                norm_bbox: [0.10, 0.12, 0.21, 0.32],
                area_px: 1296,
            },
            // Lower score AND smaller area amazon attribute does not overwrite existing 0.88
            TrackedAttribute {
                label: "amazon".to_string(),
                score: 0.65,
                bbox: BoundingBox::new(42, 62, 62, 82),
                norm_bbox: [0.13, 0.34, 0.19, 0.45],
                area_px: 400,
            },
        ];

        let enriched_active = enrich_tracked_object(TrackEnrichmentInput {
            trk: &trk,
            prev: Some(&prev),
            lpr_consensus: None,
            face_match: Some(("Alice".to_string(), 0.93)),
            frame: &frame,
            cam_cfg: &cam_cfg,
            active_profile: "armed",
            camera_suppressed_in_profile: false,
            alerts_enabled: true,
            suppressed_fp: false,
            is_ended: false,
        });

        assert!(!enriched_active.is_new);
        assert_eq!(enriched_active.newly_entered_zones, vec!["front_steps"]);
        assert!((enriched_active.best_score - 0.92).abs() < 1e-5);
        assert_eq!(
            enriched_active.effective_sub_label.as_deref(),
            Some("Alice")
        );
        assert_eq!(enriched_active.effective_sub_label_score, Some(0.93));
        assert_eq!(enriched_active.merged_attributes.len(), 2);
        let merged_face = enriched_active
            .merged_attributes
            .iter()
            .find(|a| a.label == "face")
            .unwrap();
        assert!((merged_face.score - 0.94).abs() < 1e-5);
        let merged_amazon = enriched_active
            .merged_attributes
            .iter()
            .find(|a| a.label == "amazon")
            .unwrap();
        assert!((merged_amazon.score - 0.88).abs() < 1e-5);
        assert_eq!(enriched_active.dominant_color_tag, "color:red");
        assert_eq!(
            enriched_active.attribute_tags,
            vec!["color:red", "face", "amazon"]
        );
        assert_eq!(enriched_active.face_box, Some([0.10, 0.12, 0.21, 0.32]));
        assert_eq!(enriched_active.severity, "alert");

        let active_dto = build_track_event_dto(BuildTrackEventDtoParams {
            camera: "front_door",
            trk: &trk,
            enriched: &enriched_active,
            record_active: true,
            snapshots_active: true,
            suppressed_fp: false,
            matched_fp_sig_id: None,
            severity: enriched_active.severity,
            thumbnail: Some("dGh1bWI=".to_string()),
            end_time: None,
            night_mode: Some(false),
        });
        assert!(active_dto.end_time.is_none());
        assert_eq!(active_dto.sub_label.as_deref(), Some("Alice"));
        assert_eq!(active_dto.thumbnail.as_deref(), Some("dGh1bWI="));
        assert_eq!(active_dto.data["night_mode"], json!(false));
        assert_eq!(active_dto.data["max_severity"], json!("alert"));

        // 2. LPR consensus overrides vehicle sub_label, appends plate tag, and ended track DTO sets end_time
        let mut car_trk = sample_track();
        car_trk.label = "car".to_string();
        car_trk.inferred_sub_label = Some("RAW1234".to_string());
        car_trk.attributes = vec![TrackedAttribute {
            label: "license_plate".to_string(),
            score: 0.95,
            bbox: BoundingBox::new(50, 100, 110, 124),
            norm_bbox: [0.15, 0.55, 0.34, 0.68],
            area_px: 1440,
        }];
        let lpr = LprConsensusResult {
            plate: "ABC1D23".to_string(),
            confidence: 0.96,
            sub_label: Some("Family SUV".to_string()),
            votes: 3,
        };
        let enriched_ended = enrich_tracked_object(TrackEnrichmentInput {
            trk: &car_trk,
            prev: Some(&prev),
            lpr_consensus: Some(&lpr),
            face_match: None,
            frame: &frame,
            cam_cfg: &cam_cfg,
            active_profile: "armed",
            camera_suppressed_in_profile: false,
            alerts_enabled: true,
            suppressed_fp: false,
            is_ended: true,
        });
        assert_eq!(
            enriched_ended.recognized_license_plate.as_deref(),
            Some("ABC1D23")
        );
        assert_eq!(
            enriched_ended.effective_sub_label.as_deref(),
            Some("Family SUV")
        );
        assert_eq!(enriched_ended.crop_dhash64, 0x1122_3344_5566_7788);
        assert!(
            enriched_ended
                .attribute_tags
                .contains(&"ABC1D23".to_string())
        );
        assert_eq!(
            enriched_ended.license_plate_box,
            Some([0.15, 0.55, 0.34, 0.68])
        );

        let ended_dto = build_track_event_dto(BuildTrackEventDtoParams {
            camera: "front_door",
            trk: &car_trk,
            enriched: &enriched_ended,
            record_active: true,
            snapshots_active: true,
            suppressed_fp: false,
            matched_fp_sig_id: None,
            severity: enriched_ended.severity,
            thumbnail: None,
            end_time: Some(car_trk.last_seen_time),
            night_mode: None,
        });
        assert_eq!(ended_dto.end_time, Some(1005.0));
        assert!(ended_dto.thumbnail.is_none());
        assert!(ended_dto.data.get("night_mode").is_none());
        assert_eq!(ended_dto.data["recognized_license_plate"], json!("ABC1D23"));
    }

    #[test]
    fn prune_false_positive_from_active_review_handles_retention_demotion_and_empty_deletion() {
        // Case 1: Removing one FP detection when another alert track remains keeps "alert" severity
        let mut active_review = Some(sample_active_review(
            vec!["trk-alert-1", "trk-fp-2"],
            "alert",
            false,
        ));
        let tr1 = prune_false_positive_from_active_review(
            &mut active_review,
            "trk-fp-2",
            "front_door",
            1005.0,
            |remaining| remaining.iter().any(|id| id == "trk-alert-1"),
        );
        match tr1 {
            ReviewFalsePositiveTransition::RetainedWithUpdatedSeverity { new_severity } => {
                assert_eq!(new_severity, "alert");
            }
            other => panic!("expected RetainedWithUpdatedSeverity, got {other:?}"),
        }
        assert_eq!(
            active_review.as_ref().unwrap().detections,
            vec!["trk-alert-1".to_string()]
        );

        // Case 2: Removing the sole alerting track when only a non-alert detection remains demotes "alert" -> "detection"
        let mut active_review_demote = Some(sample_active_review(
            vec!["trk-alert-1", "trk-cat-det"],
            "alert",
            false,
        ));
        let tr2 = prune_false_positive_from_active_review(
            &mut active_review_demote,
            "trk-alert-1",
            "front_door",
            1006.0,
            |_remaining| false,
        );
        match tr2 {
            ReviewFalsePositiveTransition::RetainedWithUpdatedSeverity { new_severity } => {
                assert_eq!(new_severity, "detection");
            }
            other => panic!("expected RetainedWithUpdatedSeverity, got {other:?}"),
        }
        let rev_after_demote = active_review_demote.as_ref().unwrap();
        assert_eq!(rev_after_demote.severity, "detection");
        assert_eq!(rev_after_demote.detections, vec!["trk-cat-det".to_string()]);
        assert!(rev_after_demote.dirty);

        // Case 3: Removing the last track in a persisted review clears active_review and returns DeleteEmptyReview
        let tr3 = prune_false_positive_from_active_review(
            &mut active_review_demote,
            "trk-cat-det",
            "front_door",
            1007.0,
            |_| unreachable!("not called when detections is empty"),
        );
        match tr3 {
            ReviewFalsePositiveTransition::DeleteEmptyReview {
                review_id,
                persisted_in_db,
                closed_dto,
                ..
            } => {
                assert_eq!(review_id, "1000.000000-rev01");
                assert!(persisted_in_db);
                assert_eq!(closed_dto.end_time, Some(1007.0));
                assert!(closed_dto.data["detections"].as_array().unwrap().is_empty());
            }
            other => panic!("expected DeleteEmptyReview, got {other:?}"),
        }
        assert!(active_review_demote.is_none());
    }

    #[test]
    fn profile_override_and_severity_resolution_respects_case_zones_and_sub_label_filters() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.review.alerts.enabled = true;
        cam_cfg.review.alerts.labels = vec!["person".to_string()];
        cam_cfg.review.alerts.required_zones = vec!["driveway".to_string()];
        cam_cfg.review.alerts.exclude_sub_labels = vec!["Resident".to_string()];

        let mut cfg =
            NinjaConfig::from_yaml_str("version: 0.18-0\n", PathBuf::from("/tmp/c.yml")).unwrap();
        cfg.profiles.insert(
            "NightWatch".to_string(),
            ProfileOverrideConfig {
                disabled_alerts_cameras: vec!["front_door".to_string()],
                disabled_record_cameras: Vec::new(),
            },
        );
        assert!(lookup_profile_override(&cfg, "nightwatch").is_some());
        assert!(is_camera_alert_suppressed_in_profile(
            &cfg,
            "nightwatch",
            "front_door"
        ));
        assert!(!is_camera_alert_suppressed_in_profile(
            &cfg,
            "nightwatch",
            "back_yard"
        ));

        let mut trk = sample_track();
        // Outside required zone -> detection
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk, "armed", false),
            "detection"
        );
        // Inside required zone -> alert
        trk.entered_zones = vec!["driveway".to_string()];
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk, "armed", false),
            "alert"
        );
        // Suppressed by profile -> detection
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk, "nightwatch", true),
            "detection"
        );
        // Excluded sub_label -> detection
        trk.inferred_sub_label = Some("resident".to_string());
        assert_eq!(
            compute_track_severity_for_profile(&cam_cfg, &trk, "armed", false),
            "detection"
        );
    }

    #[test]
    fn stationary_cutoff_and_60s_alert_backup_checkpoint_gating() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.review.alerts.cutoff_time = 20;
        cam_cfg.record.alert_backup.enabled = true;
        cam_cfg.record.alert_backup.severities = vec!["alert".to_string()];
        cam_cfg.record.alert_backup.target_dir = Some(PathBuf::from("/tmp/alert-backups"));

        let mut rev = sample_active_review(vec!["trk-1"], "alert", false);
        rev.dirty = true;
        rev.start_time = 1000.0;
        rev.last_activity_time = 1005.0;
        rev.last_backup_time = 1000.0;

        let mut moving_trk = sample_track();
        moving_trk.stationary = false;

        // 1. While a non-stationary track is active at t=1030 (25s > 20s cutoff), review does NOT expire
        let dec_moving = evaluate_review_cutoff_and_checkpoint(
            &rev,
            &cam_cfg,
            std::slice::from_ref(&moving_trk),
            1030.0,
        );
        assert!(!dec_moving.is_expired);
        assert_eq!(dec_moving.ws_msg_type, "update");
        assert!(!dec_moving.reached_60s_checkpoint);
        assert!(!dec_moving.should_spawn_alert_backup);

        // 2. At t=1060.0 (60s since last_backup_time) while dirty, triggers 60-second alert backup checkpoint
        let dec_60s = evaluate_review_cutoff_and_checkpoint(
            &rev,
            &cam_cfg,
            std::slice::from_ref(&moving_trk),
            1060.0,
        );
        assert!(!dec_60s.is_expired);
        assert!(dec_60s.reached_60s_checkpoint);
        assert!(dec_60s.should_advance_backup_time);
        assert!(dec_60s.should_spawn_alert_backup);

        // 3. When privacy mask is configured, alert backup is suppressed even at checkpoint
        let mut masked_cfg = cam_cfg.clone();
        masked_cfg.privacy_mask = json!(["0.0,0.0,0.5,0.0,0.5,0.5,0.0,0.5"]);
        let dec_masked = evaluate_review_cutoff_and_checkpoint(
            &rev,
            &masked_cfg,
            std::slice::from_ref(&moving_trk),
            1060.0,
        );
        assert!(!dec_masked.should_spawn_alert_backup);

        // 4. When all active tracks are stationary and (now - last_activity_time) >= cutoff_secs, review expires
        let mut stat_trk = moving_trk;
        stat_trk.stationary = true;
        rev.dirty = false;
        let dec_stat_expired = evaluate_review_cutoff_and_checkpoint(
            &rev,
            &cam_cfg,
            std::slice::from_ref(&stat_trk),
            1026.0,
        );
        assert!(dec_stat_expired.is_expired);
        assert!(dec_stat_expired.should_flush);
        assert_eq!(dec_stat_expired.end_time, Some(1005.0));
        assert_eq!(dec_stat_expired.ws_msg_type, "end");
        assert!(dec_stat_expired.should_spawn_alert_backup);

        // 5. Already-stationary non-new track must not open a brand-new review after expiration
        assert!(!should_include_track_in_review(
            &cam_cfg,
            &stat_trk,
            "alert",
            false,
            false,
            &[],
        ));
        assert!(should_include_track_in_review(
            &cam_cfg,
            &stat_trk,
            "alert",
            false,
            false,
            &["new_zone".to_string()],
        ));
    }
}
