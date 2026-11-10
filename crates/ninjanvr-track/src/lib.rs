#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use ninjanvr_config::{CameraConfig, LineConfig, LineDirection, SnapshotSelectionPolicy};
use ninjanvr_infer::{DetectionResult, nms_detections};
use ninjanvr_vision::{BoundingBox, PolygonMask};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerProvenance {
    pub trigger_ts: f64,
    pub bbox: [u32; 4],
    pub norm_bbox: [f64; 4],
    pub area_px: u32,
    pub area_pct: f32,
    pub aspect_ratio: f32,
    pub score: f32,
    pub top_score: f32,
    pub min_score_cfg: f32,
    pub threshold_cfg: f32,
    pub zones: Vec<String>,
    pub frames_tracked: u32,
    pub stationary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackedAttribute {
    pub label: String,
    pub score: f32,
    pub bbox: BoundingBox,
    pub norm_bbox: [f64; 4],
    pub area_px: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedObject {
    pub id: String,
    pub label: String,
    pub score: f32,
    pub top_score: f32,
    pub bbox: BoundingBox,
    pub region: BoundingBox,
    pub start_time: f64,
    pub last_seen_time: f64,
    pub frames_tracked: u32,
    pub disappeared_frames: u32,
    pub motionless_count: u32,
    pub stationary: bool,
    pub zones: Vec<String>,
    pub entered_zones: Vec<String>,
    #[serde(default)]
    pub crossed_lines: Vec<String>,
    #[serde(default)]
    pub prev_bottom_center: (f32, f32),
    pub has_snapshot: bool,
    pub vx: f32,
    pub vy: f32,
    pub provenance: TriggerProvenance,
    #[serde(default)]
    pub attributes: Vec<TrackedAttribute>,
    #[serde(default)]
    pub best_attribute_score: f32,
    #[serde(default)]
    pub inferred_sub_label: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct ZonePresenceState {
    consecutive_in_frames: u32,
    first_entered_ts: f64,
    has_finite_ts: bool,
}

const MAX_ACTIVE_TRACKS: usize = 256;
const MAX_ATTRIBUTES_PER_TRACK: usize = 32;

/// Multi-Object Tracker implementing 2-stage **ByteTrack** association
/// (high-confidence pass + low-confidence occlusion recovery pass),
/// constant-velocity Kalman state estimation, Stationary object classification,
/// and polygonal Zone / Object Mask evaluation.
pub struct ByteTracker {
    camera_name: String,
    frame_w: u32,
    frame_h: u32,
    min_initialized: u32,
    min_initialized_night: u32,
    night_mode: bool,
    active_preset: Option<String>,
    max_disappeared: u32,
    stationary_threshold: u32,
    stationary_interval: u32,
    global_object_mask: PolygonMask,
    label_masks: BTreeMap<String, PolygonMask>,
    zones: BTreeMap<String, (PolygonMask, Vec<String>, Option<String>)>,
    zone_presence: BTreeMap<String, BTreeMap<String, ZonePresenceState>>,
    unscanned_stationary_frames: BTreeMap<String, u32>,
    confirmed_tracks: BTreeSet<String>,
    tracks: BTreeMap<String, TrackedObject>,
    frame_seq: u64,
    next_track_seq: u32,
    last_frame_time: f64,
    has_finite_frame_time: bool,
}

type ZonePolygonMap = BTreeMap<String, (PolygonMask, Vec<String>, Option<String>)>;

fn build_object_masks(
    cfg: &CameraConfig,
    frame_w: u32,
    frame_h: u32,
) -> (PolygonMask, BTreeMap<String, PolygonMask>) {
    let global_object_mask = PolygonMask::from_config_value(&cfg.objects.mask, frame_w, frame_h);
    let mut label_masks = BTreeMap::new();
    for (label, f) in &cfg.objects.filters {
        let m = PolygonMask::from_config_value(&f.mask, frame_w, frame_h);
        if !m.is_empty() {
            label_masks.insert(label.clone(), m);
        }
    }
    (global_object_mask, label_masks)
}

fn build_zone_polygons(cfg: &CameraConfig, frame_w: u32, frame_h: u32) -> ZonePolygonMap {
    let mut zones = BTreeMap::new();
    for (zname, zcfg) in &cfg.zones {
        let poly = PolygonMask::from_config_value(&zcfg.coordinates, frame_w, frame_h);
        if !poly.is_empty() {
            let preset = zcfg
                .preset
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            zones.insert(zname.clone(), (poly, zcfg.objects.clone(), preset));
        }
    }
    zones
}

impl ByteTracker {
    pub fn new(
        camera_name: impl Into<String>,
        frame_w: u32,
        frame_h: u32,
        cfg: &CameraConfig,
    ) -> Self {
        let frame_w = frame_w.max(1);
        let frame_h = frame_h.max(1);
        let (global_object_mask, label_masks) = build_object_masks(cfg, frame_w, frame_h);
        let zones = build_zone_polygons(cfg, frame_w, frame_h);

        let min_init = cfg.detect.min_initialized.max(1);
        let min_init_night = cfg
            .detect
            .min_initialized_night
            .unwrap_or(min_init.saturating_add(2))
            .max(min_init);
        let initial_preset = cfg
            .onvif
            .return_preset
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        Self {
            camera_name: camera_name.into(),
            frame_w,
            frame_h,
            min_initialized: min_init,
            min_initialized_night: min_init_night,
            night_mode: false,
            active_preset: initial_preset,
            max_disappeared: cfg.detect.max_disappeared.max(5),
            stationary_threshold: cfg.detect.stationary.threshold.max(10),
            stationary_interval: cfg.detect.stationary.interval.max(10),
            global_object_mask,
            label_masks,
            zones,
            zone_presence: BTreeMap::new(),
            unscanned_stationary_frames: BTreeMap::new(),
            confirmed_tracks: BTreeSet::new(),
            tracks: BTreeMap::new(),
            frame_seq: 0,
            next_track_seq: 0,
            last_frame_time: 0.0,
            has_finite_frame_time: false,
        }
    }

    /// Applies edited zone polygons and object masks from a live config change without
    /// dropping active tracks. Zone presence for removed or redrawn zones is reset so the
    /// inertia/loitering hysteresis restarts against the new geometry.
    pub fn reconfigure_regions(&mut self, cfg: &CameraConfig) {
        let (global_object_mask, label_masks) = build_object_masks(cfg, self.frame_w, self.frame_h);
        let next_zones = build_zone_polygons(cfg, self.frame_w, self.frame_h);
        let unchanged: BTreeSet<String> = next_zones
            .iter()
            .filter(|(name, (poly, _, _))| {
                self.zones
                    .get(*name)
                    .is_some_and(|(old_poly, _, _)| old_poly == poly)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for presence in self.zone_presence.values_mut() {
            presence.retain(|zone, _| unchanged.contains(zone));
        }
        self.global_object_mask = global_object_mask;
        self.label_masks = label_masks;
        self.zones = next_zones;
    }

    pub fn set_night_mode(&mut self, night_mode: bool) {
        self.night_mode = night_mode;
    }

    pub fn is_night_mode(&self) -> bool {
        self.night_mode
    }

    pub fn set_active_preset(&mut self, preset: Option<&str>) {
        self.active_preset = preset
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }

    pub fn active_preset(&self) -> Option<&str> {
        self.active_preset.as_deref()
    }

    fn effective_min_initialized(&self) -> u32 {
        if self.night_mode {
            self.min_initialized_night
        } else {
            self.min_initialized
        }
    }

    pub fn active_tracks(&self) -> impl Iterator<Item = &TrackedObject> {
        let req_frames = self.effective_min_initialized();
        let confirmed = &self.confirmed_tracks;
        self.tracks.values().filter(move |t| {
            (confirmed.contains(&t.id) || t.frames_tracked >= req_frames)
                && t.disappeared_frames == 0
        })
    }

    pub fn non_stationary_boxes(&self, motion_boxes: &[BoundingBox]) -> Vec<BoundingBox> {
        let force_scan = self
            .frame_seq
            .is_multiple_of(self.stationary_interval as u64);
        self.tracks
            .values()
            .filter(|t| {
                t.disappeared_frames <= 5
                    && (force_scan
                        || !t.stationary
                        || t.disappeared_frames > 0
                        || motion_boxes.iter().any(|mb| mb.intersects(&t.bbox)))
            })
            .map(|t| t.bbox)
            .collect()
    }

    fn clamped_bottom_center(&self, bbox: &BoundingBox) -> (f32, f32) {
        let (bx, by) = bbox.bottom_center();
        let max_x = (self.frame_w as f32 - 1e-3).max(0.0);
        let max_y = (self.frame_h as f32 - 1e-3).max(0.0);
        (bx.clamp(0.0, max_x), by.clamp(0.0, max_y))
    }

    fn is_attribute_masked_or_filtered(&self, det: &DetectionResult, cfg: &CameraConfig) -> bool {
        if !det.score.is_finite()
            || det.score < 0.0
            || det.bbox.width() == 0
            || det.bbox.height() == 0
        {
            return true;
        }
        let (bx, by) = self.clamped_bottom_center(&det.bbox);
        if self.global_object_mask.contains_point(bx, by) {
            return true;
        }
        if let Some(lm) = self.label_masks.get(&det.label).or_else(|| {
            self.label_masks
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&det.label))
                .map(|(_, v)| v)
        }) && lm.contains_point(bx, by)
        {
            return true;
        }
        if let Some(f) = cfg.objects.filters.get(&det.label).or_else(|| {
            cfg.objects
                .filters
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&det.label))
                .map(|(_, v)| v)
        }) {
            let area = det.bbox.area();
            let (min_area_px, max_area_px) = f.resolve_area_bounds_px(self.frame_w, self.frame_h);
            if area < min_area_px || area > max_area_px {
                return true;
            }
            let ratio = (det.bbox.width() as f32) / (det.bbox.height().max(1) as f32);
            if (f.min_ratio.is_finite() && ratio < f.min_ratio)
                || (f.max_ratio.is_finite() && ratio > f.max_ratio)
            {
                return true;
            }
            if f.min_score.is_finite() && det.score < f.min_score {
                return true;
            }
        }
        false
    }

    fn is_detection_masked_or_filtered(&self, det: &DetectionResult, cfg: &CameraConfig) -> bool {
        if !det.score.is_finite()
            || det.score < 0.0
            || det.bbox.width() == 0
            || det.bbox.height() == 0
        {
            return true;
        }
        if !cfg.objects.track.iter().any(|l| l == &det.label) {
            return true;
        }
        let (bx, by) = self.clamped_bottom_center(&det.bbox);
        if self.global_object_mask.contains_point(bx, by) {
            return true;
        }
        if let Some(lm) = self.label_masks.get(&det.label)
            && lm.contains_point(bx, by)
        {
            return true;
        }
        if let Some(f) = cfg.objects.filters.get(&det.label) {
            let area = det.bbox.area();
            let (min_area_px, max_area_px) = f.resolve_area_bounds_px(self.frame_w, self.frame_h);
            if area < min_area_px || area > max_area_px {
                return true;
            }
            let ratio = (det.bbox.width() as f32) / (det.bbox.height().max(1) as f32);
            if (f.min_ratio.is_finite() && ratio < f.min_ratio)
                || (f.max_ratio.is_finite() && ratio > f.max_ratio)
            {
                return true;
            }
            if f.min_score.is_finite() && det.score < f.min_score {
                return true;
            }
        }
        false
    }

    fn evaluate_zones(
        &self,
        label: &str,
        bbox: &BoundingBox,
        score: f32,
        cfg: &CameraConfig,
    ) -> Vec<String> {
        if !score.is_finite() {
            return Vec::new();
        }
        let (bx, by) = self.clamped_bottom_center(bbox);
        let area = bbox.area();
        let frame_area = (self.frame_w as f64) * (self.frame_h as f64);
        let mut matched = Vec::new();
        for (zname, (poly, allowed_objs, cached_preset)) in &self.zones {
            if (!allowed_objs.is_empty() && !allowed_objs.iter().any(|o| o == label))
                || !poly.contains_point(bx, by)
            {
                continue;
            }
            let zcfg_opt = cfg.zones.get(zname);
            let req_preset = zcfg_opt
                .and_then(|zc| zc.preset.as_deref())
                .or(cached_preset.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if let Some(req_p) = req_preset
                && self.active_preset() != Some(req_p)
            {
                continue;
            }
            if let Some(zcfg) = zcfg_opt {
                if let Some(min_sc) = zcfg.min_score
                    && min_sc.is_finite()
                    && score < min_sc
                {
                    continue;
                }
                if let Some(min_a) = zcfg.min_area
                    && min_a.is_finite()
                {
                    let min_px = if min_a > 0.0 && min_a <= 1.0 {
                        (min_a * frame_area).round() as u32
                    } else {
                        min_a.max(0.0).round() as u32
                    };
                    if area < min_px {
                        continue;
                    }
                }
                if let Some(max_a) = zcfg.max_area
                    && max_a.is_finite()
                    && max_a > 0.0
                {
                    let max_px = if max_a <= 1.0 {
                        (max_a * frame_area).round() as u32
                    } else {
                        max_a.max(1.0).round() as u32
                    };
                    if area > max_px {
                        continue;
                    }
                }
            }
            matched.push(zname.clone());
        }
        matched
    }

    fn resolve_zones_with_hysteresis(
        &mut self,
        tid: &str,
        frame_time: f64,
        raw_zones: &[String],
        cfg: &CameraConfig,
    ) -> Vec<String> {
        if self.zones.is_empty() {
            return Vec::new();
        }
        let time_is_finite = frame_time.is_finite();
        let safe_time = if time_is_finite {
            frame_time
        } else if self.has_finite_frame_time {
            self.last_frame_time
        } else {
            0.0
        };
        let track_zone_map = self.zone_presence.entry(tid.to_string()).or_default();
        let mut confirmed = Vec::new();
        for zname in self.zones.keys() {
            let in_raw = raw_zones.iter().any(|z| z == zname);
            let (req_inertia, req_loiter) = cfg
                .zones
                .get(zname)
                .map_or((1, 0.0), |zc| (zc.inertia.max(1), zc.loitering_time as f64));
            if in_raw {
                let st = track_zone_map.entry(zname.clone()).or_default();
                if st.consecutive_in_frames == 0
                    || !st.first_entered_ts.is_finite()
                    || (time_is_finite && (!st.has_finite_ts || safe_time < st.first_entered_ts))
                {
                    st.first_entered_ts = safe_time;
                    if time_is_finite {
                        st.has_finite_ts = true;
                    }
                }
                st.consecutive_in_frames = st.consecutive_in_frames.saturating_add(1);
                let inertia_ok = st.consecutive_in_frames >= req_inertia;
                let loiter_ok = req_loiter <= 0.0
                    || (st.has_finite_ts && (safe_time - st.first_entered_ts) >= req_loiter);
                if inertia_ok && loiter_ok {
                    confirmed.push(zname.clone());
                }
            } else if let Some(st) = track_zone_map.get_mut(zname) {
                st.consecutive_in_frames = 0;
                st.has_finite_ts = false;
            }
        }
        confirmed
    }

    /// Updates tracks using cross-region NMS + 2-stage ByteTrack association
    /// and returns `(updated_active_tracks, ended_tracks)`.
    pub fn update(
        &mut self,
        frame_time: f64,
        raw_detections: &[DetectionResult],
        scanned_regions: &[BoundingBox],
        cfg: &CameraConfig,
    ) -> (Vec<TrackedObject>, Vec<TrackedObject>) {
        self.frame_seq = self.frame_seq.wrapping_add(1);
        let raw_time_finite = frame_time.is_finite();
        let eval_time = if raw_time_finite {
            self.last_frame_time = frame_time;
            self.has_finite_frame_time = true;
            frame_time
        } else {
            f64::NAN
        };
        let frame_time = if raw_time_finite {
            frame_time
        } else if self.has_finite_frame_time {
            self.last_frame_time
        } else {
            0.0
        };

        let sanitized_raw: Vec<DetectionResult> = raw_detections
            .iter()
            .filter_map(|d| {
                if !d.score.is_finite() || d.score < 0.0 {
                    return None;
                }
                let x_min = d.bbox.x_min.min(d.bbox.x_max);
                let y_min = d.bbox.y_min.min(d.bbox.y_max);
                let x_max = d.bbox.x_max.max(d.bbox.x_min).min(self.frame_w);
                let y_max = d.bbox.y_max.max(d.bbox.y_min).min(self.frame_h);
                if x_max <= x_min || y_max <= y_min {
                    return None;
                }
                let mut clean = d.clone();
                clean.bbox = BoundingBox::new(x_min, y_min, x_max, y_max);
                let is_attr = cfg
                    .objects
                    .attribute_labels
                    .iter()
                    .any(|attr_lbl| attr_lbl.eq_ignore_ascii_case(&clean.label));
                if is_attr {
                    if self.is_attribute_masked_or_filtered(&clean, cfg) {
                        return None;
                    }
                } else if self.is_detection_masked_or_filtered(&clean, cfg) {
                    return None;
                }
                Some(clean)
            })
            .collect();
        let deduplicated = nms_detections(sanitized_raw, 0.55);

        // 1. Predict next state for existing tracks using constant-velocity model
        for trk in self.tracks.values_mut() {
            if !trk.vx.is_finite() || trk.stationary {
                trk.vx = 0.0;
            }
            if !trk.vy.is_finite() || trk.stationary {
                trk.vy = 0.0;
            }
            if !trk.stationary
                && trk.disappeared_frames > 0
                && (trk.vx.abs() > 0.5 || trk.vy.abs() > 0.5)
            {
                let w = trk.bbox.width().min(self.frame_w);
                let h = trk.bbox.height().min(self.frame_h);
                let max_x = self.frame_w.saturating_sub(w) as f32;
                let max_y = self.frame_h.saturating_sub(h) as f32;
                let nx1 = ((trk.bbox.x_min as f32) + trk.vx).clamp(0.0, max_x) as u32;
                let ny1 = ((trk.bbox.y_min as f32) + trk.vy).clamp(0.0, max_y) as u32;
                trk.bbox = BoundingBox::new(nx1, ny1, nx1 + w, ny1 + h);
            }
        }

        // 2. Partition detections into Attribute Detections vs Primary High/Low-Score Detections
        let mut attribute_dets = Vec::new();
        let mut high_dets = Vec::new();
        let mut low_dets = Vec::new();
        for det in deduplicated {
            if cfg
                .objects
                .attribute_labels
                .iter()
                .any(|attr_lbl| attr_lbl.eq_ignore_ascii_case(&det.label))
            {
                if !self.is_attribute_masked_or_filtered(&det, cfg) {
                    attribute_dets.push(det);
                }
                continue;
            }
            if self.is_detection_masked_or_filtered(&det, cfg) {
                continue;
            }
            let raw_high_thresh = cfg.objects.filters.get(&det.label).map_or(0.7, |f| {
                if self.night_mode {
                    f.night_threshold.unwrap_or(f.threshold)
                } else {
                    f.threshold
                }
            });
            let high_thresh = if raw_high_thresh.is_finite() {
                raw_high_thresh
            } else {
                0.7
            };
            if det.score >= high_thresh {
                high_dets.push(det);
            } else {
                low_dets.push(det);
            }
        }

        let track_ids: Vec<String> = self.tracks.keys().cloned().collect();
        let mut matched_tracks = vec![false; track_ids.len()];
        let mut matched_high = vec![false; high_dets.len()];

        // Stage 1: Globally rank and associate High-Confidence detections with existing tracks
        let mut stage1_pairs: Vec<(f32, usize, usize)> = Vec::new();
        for (d_idx, det) in high_dets.iter().enumerate() {
            for (t_idx, tid) in track_ids.iter().enumerate() {
                let trk = &self.tracks[tid];
                if trk.label != det.label {
                    continue;
                }
                let sim = association_similarity(&trk.bbox, &det.bbox);
                if sim > 0.15 {
                    stage1_pairs.push((sim, d_idx, t_idx));
                }
            }
        }
        stage1_pairs.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let mut stage1_matches: Vec<(usize, usize)> = Vec::new();
        for (_, d_idx, t_idx) in stage1_pairs {
            if !matched_high[d_idx] && !matched_tracks[t_idx] {
                matched_high[d_idx] = true;
                matched_tracks[t_idx] = true;
                stage1_matches.push((d_idx, t_idx));
            }
        }
        for (d_idx, t_idx) in stage1_matches {
            let tid = &track_ids[t_idx];
            self.unscanned_stationary_frames.remove(tid);
            self.apply_match(tid, &high_dets[d_idx], eval_time, frame_time, cfg);
        }

        // Stage 2 (ByteTrack): Globally rank and associate Low-Confidence detections with remaining unmatched tracks
        let mut matched_low = vec![false; low_dets.len()];
        let mut stage2_pairs: Vec<(f32, usize, usize)> = Vec::new();
        for (d_idx, det) in low_dets.iter().enumerate() {
            for (t_idx, tid) in track_ids.iter().enumerate() {
                if matched_tracks[t_idx] {
                    continue;
                }
                let trk = &self.tracks[tid];
                if trk.label != det.label || trk.disappeared_frames > 5 {
                    continue;
                }
                let iou = trk.bbox.iou(&det.bbox);
                if iou > 0.30 {
                    stage2_pairs.push((iou, d_idx, t_idx));
                }
            }
        }
        stage2_pairs.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let mut stage2_matches: Vec<(usize, usize)> = Vec::new();
        for (_, d_idx, t_idx) in stage2_pairs {
            if !matched_low[d_idx] && !matched_tracks[t_idx] {
                matched_low[d_idx] = true;
                matched_tracks[t_idx] = true;
                stage2_matches.push((d_idx, t_idx));
            }
        }
        for (d_idx, t_idx) in stage2_matches {
            let tid = &track_ids[t_idx];
            self.unscanned_stationary_frames.remove(tid);
            self.apply_match(tid, &low_dets[d_idx], eval_time, frame_time, cfg);
        }

        // 3. Handle unmatched tracks (preserving stationary tracks that were not in any scanned region,
        // bounding unscanned stationary retention, and immediately dropping unconfirmed tentative tracks)
        let req_init = self.effective_min_initialized();
        let max_stationary_unscanned = self
            .stationary_interval
            .saturating_mul(12)
            .max(self.max_disappeared.saturating_mul(10))
            .max(120);
        let mut ended_tracks = Vec::new();
        for (t_idx, tid) in track_ids.iter().enumerate() {
            if !matched_tracks[t_idx] {
                let is_confirmed = self.confirmed_tracks.contains(tid)
                    || self
                        .tracks
                        .get(tid)
                        .is_some_and(|t| t.frames_tracked >= req_init);
                let mut drop_unconfirmed = false;
                if let Some(trk) = self.tracks.get_mut(tid) {
                    if !is_confirmed {
                        drop_unconfirmed = true;
                    } else {
                        let was_scanned = scanned_regions.iter().any(|r| r.intersects(&trk.bbox));
                        if !trk.stationary || was_scanned {
                            self.unscanned_stationary_frames.remove(tid);
                            trk.disappeared_frames = trk.disappeared_frames.saturating_add(1);
                            if trk.stationary && trk.disappeared_frames >= 3 {
                                trk.stationary = false;
                            }
                        } else {
                            let unscanned = self
                                .unscanned_stationary_frames
                                .entry(tid.clone())
                                .or_insert(0);
                            *unscanned = unscanned.saturating_add(1);
                            if *unscanned > max_stationary_unscanned {
                                trk.stationary = false;
                                trk.disappeared_frames = self.max_disappeared.saturating_add(1);
                            }
                        }
                    }
                }
                if drop_unconfirmed {
                    self.tracks.remove(tid);
                    self.confirmed_tracks.remove(tid);
                    self.zone_presence.remove(tid);
                    self.unscanned_stationary_frames.remove(tid);
                } else if self
                    .tracks
                    .get(tid)
                    .is_some_and(|t| t.disappeared_frames > self.max_disappeared)
                    && let Some(removed) = self.tracks.remove(tid)
                {
                    let was_confirmed =
                        self.confirmed_tracks.remove(tid) || removed.frames_tracked >= req_init;
                    self.zone_presence.remove(tid);
                    self.unscanned_stationary_frames.remove(tid);
                    if was_confirmed {
                        ended_tracks.push(removed);
                    }
                }
            }
        }

        // 4. Initialize new tracks from unmatched High-Confidence detections
        for (d_idx, det) in high_dets.into_iter().enumerate() {
            if matched_high[d_idx] {
                continue;
            }
            // Skip if it heavily overlaps or is nested inside an already-active track of the same label
            if self.tracks.values().any(|t| {
                t.label == det.label
                    && (t.bbox.iou(&det.bbox) > 0.55
                        || t.bbox.intersection_over_min(&det.bbox) > 0.78)
            }) {
                continue;
            }
            if self.tracks.len() >= MAX_ACTIVE_TRACKS {
                let confirmed_set = &self.confirmed_tracks;
                let Some(evict_id) = self
                    .tracks
                    .iter()
                    .filter(|(k, t)| {
                        let is_conf = confirmed_set.contains(*k) || t.frames_tracked >= req_init;
                        !is_conf || t.disappeared_frames > 0
                    })
                    .min_by(|(ka, a), (kb, b)| {
                        let a_conf = confirmed_set.contains(*ka) || a.frames_tracked >= req_init;
                        let b_conf = confirmed_set.contains(*kb) || b.frames_tracked >= req_init;
                        a_conf
                            .cmp(&b_conf)
                            .then_with(|| b.disappeared_frames.cmp(&a.disappeared_frames))
                            .then_with(|| a.last_seen_time.total_cmp(&b.last_seen_time))
                            .then_with(|| a.frames_tracked.cmp(&b.frames_tracked))
                            .then_with(|| a.top_score.total_cmp(&b.top_score))
                    })
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                if let Some(removed) = self.tracks.remove(&evict_id) {
                    let was_confirmed = self.confirmed_tracks.remove(&evict_id)
                        || removed.frames_tracked >= req_init;
                    self.zone_presence.remove(&evict_id);
                    self.unscanned_stationary_frames.remove(&evict_id);
                    if was_confirmed {
                        ended_tracks.push(removed);
                    }
                }
            }
            let mut id = format!(
                "{frame_time:.6}-{:06x}",
                (self.frame_seq as u32).wrapping_mul(0x9E37_79B9) ^ (d_idx as u32)
            );
            while self.tracks.contains_key(&id) {
                self.next_track_seq = self.next_track_seq.wrapping_add(1);
                id = format!(
                    "{frame_time:.6}-{:06x}",
                    (self.frame_seq as u32)
                        .wrapping_mul(0x9E37_79B9)
                        .wrapping_add(self.next_track_seq.wrapping_mul(0x85EB_CA6B))
                        ^ (d_idx as u32)
                );
            }
            if req_init <= 1 {
                self.confirmed_tracks.insert(id.clone());
            }
            let raw_zones = self.evaluate_zones(&det.label, &det.bbox, det.score, cfg);
            let zones = self.resolve_zones_with_hysteresis(&id, eval_time, &raw_zones, cfg);
            let (raw_min_score_cfg, raw_threshold_cfg) = cfg
                .objects
                .filters
                .get(&det.label)
                .map_or((0.5, 0.7), |f| (f.min_score, f.threshold));
            let min_score_cfg = if raw_min_score_cfg.is_finite() {
                raw_min_score_cfg
            } else {
                0.5
            };
            let threshold_cfg = if raw_threshold_cfg.is_finite() {
                raw_threshold_cfg
            } else {
                0.7
            };
            let frame_area = (self.frame_w.saturating_mul(self.frame_h)).max(1) as f32;
            let area_px = det.bbox.area();
            let area_pct = ((area_px as f32 / frame_area) * 10000.0).round() / 100.0;
            let aspect_ratio =
                ((det.bbox.width() as f32 / (det.bbox.height().max(1) as f32)) * 100.0).round()
                    / 100.0;
            let provenance = TriggerProvenance {
                trigger_ts: frame_time,
                bbox: [
                    det.bbox.x_min,
                    det.bbox.y_min,
                    det.bbox.x_max,
                    det.bbox.y_max,
                ],
                norm_bbox: det.bbox.to_normalized(self.frame_w, self.frame_h),
                area_px,
                area_pct,
                aspect_ratio,
                score: det.score,
                top_score: det.score,
                min_score_cfg,
                threshold_cfg,
                zones: zones.clone(),
                frames_tracked: 1,
                stationary: false,
            };
            let _ = &self.camera_name;
            self.tracks.insert(
                id.clone(),
                TrackedObject {
                    id,
                    label: det.label,
                    score: det.score,
                    top_score: det.score,
                    bbox: det.bbox,
                    region: det.region,
                    start_time: frame_time,
                    last_seen_time: frame_time,
                    frames_tracked: 1,
                    disappeared_frames: 0,
                    motionless_count: 0,
                    stationary: false,
                    entered_zones: zones.clone(),
                    zones,
                    crossed_lines: Vec::new(),
                    prev_bottom_center: det.bbox.bottom_center(),
                    has_snapshot: false,
                    vx: 0.0,
                    vy: 0.0,
                    provenance,
                    attributes: Vec::new(),
                    best_attribute_score: 0.0,
                    inferred_sub_label: None,
                },
            );
        }

        // 5. Associate child attribute detections (face, license_plate, delivery logos) to parent tracks
        self.associate_attributes_to_tracks(&attribute_dets);
        self.confirmed_tracks
            .retain(|k| self.tracks.contains_key(k));
        self.zone_presence
            .retain(|k, _| self.tracks.contains_key(k));
        self.unscanned_stationary_frames
            .retain(|k, _| self.tracks.contains_key(k));

        let active: Vec<TrackedObject> = self.active_tracks().cloned().collect();
        (active, ended_tracks)
    }

    fn associate_attributes_to_tracks(&mut self, attribute_dets: &[DetectionResult]) {
        for trk in self.tracks.values_mut() {
            if trk.disappeared_frames == 0 {
                trk.attributes.clear();
            }
        }
        if attribute_dets.is_empty() {
            return;
        }
        let frame_w = self.frame_w;
        let frame_h = self.frame_h;
        for attr_det in attribute_dets {
            let attr_area = attr_det.bbox.area();
            if attr_area == 0 {
                continue;
            }
            let is_face = attr_det.label.eq_ignore_ascii_case("face");
            let is_plate = attr_det.label.eq_ignore_ascii_case("license_plate");
            let mut best_tid: Option<String> = None;
            let mut best_containment = 0.60f32;
            let mut best_parent_score = -1.0f32;

            for (tid, trk) in &self.tracks {
                if trk.disappeared_frames > 0 || attr_area > trk.bbox.area() {
                    continue;
                }
                let parent_eligible = if is_face {
                    trk.label == "person"
                } else if is_plate {
                    is_vehicle_label(&trk.label)
                } else {
                    is_vehicle_label(&trk.label) || trk.label == "person"
                };
                if !parent_eligible {
                    continue;
                }
                let containment = trk.bbox.intersection_over_min(&attr_det.bbox);
                if containment >= 0.60
                    && (containment > best_containment
                        || ((containment - best_containment).abs() < 1e-5
                            && trk.score > best_parent_score))
                {
                    best_containment = containment;
                    best_parent_score = trk.score;
                    best_tid = Some(tid.clone());
                }
            }

            if let Some(tid) = best_tid
                && let Some(trk) = self.tracks.get_mut(&tid)
            {
                let area_px = attr_det.bbox.area();
                let norm_bbox = attr_det.bbox.to_normalized(frame_w, frame_h);
                if is_face || attr_det.label.eq_ignore_ascii_case("license_plate") {
                    let attr_q =
                        compute_attribute_quality(attr_det.score, area_px, frame_w, frame_h);
                    if attr_q > trk.best_attribute_score {
                        trk.best_attribute_score = attr_q;
                    }
                } else if is_delivery_logo_label(&attr_det.label) {
                    let has_higher_logo = trk
                        .attributes
                        .iter()
                        .any(|a| is_delivery_logo_label(&a.label) && a.score >= attr_det.score);
                    if !has_higher_logo {
                        trk.inferred_sub_label = Some(attr_det.label.clone());
                    }
                }
                if trk.attributes.len() < MAX_ATTRIBUTES_PER_TRACK {
                    trk.attributes.push(TrackedAttribute {
                        label: attr_det.label.clone(),
                        score: attr_det.score,
                        bbox: attr_det.bbox,
                        norm_bbox,
                        area_px,
                    });
                }
            }
        }
    }

    fn apply_match(
        &mut self,
        tid: &str,
        det: &DetectionResult,
        eval_time: f64,
        frame_time: f64,
        cfg: &CameraConfig,
    ) {
        let raw_zones = self.evaluate_zones(&det.label, &det.bbox, det.score, cfg);
        let zones = self.resolve_zones_with_hysteresis(tid, eval_time, &raw_zones, cfg);
        let stationary_thresh = self.stationary_threshold;
        let frame_w = self.frame_w;
        let frame_h = self.frame_h;
        let eff_min_init = self.effective_min_initialized();
        let was_confirmed = self.confirmed_tracks.contains(tid);
        let (raw_min_score_cfg, raw_threshold_cfg) = cfg
            .objects
            .filters
            .get(&det.label)
            .map_or((0.5, 0.7), |f| (f.min_score, f.threshold));
        let min_score_cfg = if raw_min_score_cfg.is_finite() {
            raw_min_score_cfg
        } else {
            0.5
        };
        let threshold_cfg = if raw_threshold_cfg.is_finite() {
            raw_threshold_cfg
        } else {
            0.7
        };
        let mut newly_confirmed = false;
        if let Some(trk) = self.tracks.get_mut(tid) {
            let (old_cx, old_cy) = trk.bbox.center();
            let (new_cx, new_cy) = det.bbox.center();
            let was_predicted = !trk.stationary
                && trk.disappeared_frames > 0
                && (trk.vx.abs() > 0.5 || trk.vy.abs() > 0.5);
            let (dx, dy) = if was_predicted {
                ((new_cx - old_cx) + trk.vx, (new_cy - old_cy) + trk.vy)
            } else {
                (new_cx - old_cx, new_cy - old_cy)
            };
            trk.vx = 0.7 * trk.vx + 0.3 * dx;
            trk.vy = 0.7 * trk.vy + 0.3 * dy;

            let iou = trk.bbox.iou(&det.bbox);
            let max_jitter = ((frame_w.max(frame_h) as f32) * 0.02).max(6.0);
            if iou > 0.72 && dx.hypot(dy) < max_jitter {
                trk.motionless_count = trk.motionless_count.saturating_add(1);
                if trk.motionless_count >= stationary_thresh {
                    trk.stationary = true;
                    trk.vx = 0.0;
                    trk.vy = 0.0;
                }
            } else if iou < 0.55 || dx.hypot(dy) > max_jitter * 2.5 {
                trk.motionless_count = 0;
                trk.stationary = false;
            } else {
                trk.motionless_count = trk.motionless_count.saturating_sub(2);
            }

            let prev_bc = trk.prev_bottom_center;
            let new_bc = det.bbox.bottom_center();
            trk.prev_bottom_center = new_bc;

            let is_new_top = det.score > trk.top_score;
            trk.bbox = det.bbox;
            trk.region = det.region;
            trk.score = det.score;
            if is_new_top {
                trk.top_score = det.score;
            }
            trk.last_seen_time = frame_time;
            trk.frames_tracked = trk.frames_tracked.saturating_add(1);
            if trk.frames_tracked >= eff_min_init {
                newly_confirmed = !was_confirmed;
            }
            trk.disappeared_frames = 0;
            trk.zones = zones.clone();
            for z in zones {
                if !trk.entered_zones.contains(&z) {
                    trk.entered_zones.push(z);
                }
            }

            // Evaluate directional virtual tripwire crossings
            for (line_name, line_cfg) in &cfg.lines {
                if !line_cfg.objects.is_empty() && !line_cfg.objects.iter().any(|o| o == &det.label)
                {
                    continue;
                }
                let (l_start, l_end) = resolve_line_endpoints(line_cfg, frame_w, frame_h);
                if let Some(cross_dir) = classify_segment_crossing(l_start, l_end, prev_bc, new_bc)
                {
                    let matches_dir = match line_cfg.direction {
                        LineDirection::Any => true,
                        LineDirection::AToB => cross_dir == LineDirection::AToB,
                        LineDirection::BToA => cross_dir == LineDirection::BToA,
                    };
                    if matches_dir {
                        if !trk.crossed_lines.contains(line_name) {
                            trk.crossed_lines.push(line_name.clone());
                        }
                        if !trk.zones.contains(line_name) {
                            trk.zones.push(line_name.clone());
                        }
                        if !trk.entered_zones.contains(line_name) {
                            trk.entered_zones.push(line_name.clone());
                        }
                    }
                }
            }

            if is_new_top || newly_confirmed {
                let frame_area = (frame_w.saturating_mul(frame_h)).max(1) as f32;
                let area_px = det.bbox.area();
                let area_pct = ((area_px as f32 / frame_area) * 10000.0).round() / 100.0;
                let aspect_ratio =
                    ((det.bbox.width() as f32 / (det.bbox.height().max(1) as f32)) * 100.0).round()
                        / 100.0;
                trk.provenance.trigger_ts = frame_time;
                trk.provenance.bbox = [
                    det.bbox.x_min,
                    det.bbox.y_min,
                    det.bbox.x_max,
                    det.bbox.y_max,
                ];
                trk.provenance.norm_bbox = det.bbox.to_normalized(frame_w, frame_h);
                trk.provenance.area_px = area_px;
                trk.provenance.area_pct = area_pct;
                trk.provenance.aspect_ratio = aspect_ratio;
                trk.provenance.score = det.score;
            }
            trk.provenance.top_score = trk.top_score;
            trk.provenance.min_score_cfg = min_score_cfg;
            trk.provenance.threshold_cfg = threshold_cfg;
            trk.provenance.zones = trk.entered_zones.clone();
            trk.provenance.frames_tracked = trk.frames_tracked;
            trk.provenance.stationary = trk.stationary;
        }
        if was_confirmed
            || newly_confirmed
            || self
                .tracks
                .get(tid)
                .is_some_and(|t| t.frames_tracked >= eff_min_init)
        {
            self.confirmed_tracks.insert(tid.to_string());
        }
    }
}

fn resolve_line_endpoints(
    line_cfg: &LineConfig,
    frame_w: u32,
    frame_h: u32,
) -> ((f32, f32), (f32, f32)) {
    let [mut x1, mut y1, mut x2, mut y2] = line_cfg.points;
    if (0.0..=1.0).contains(&x1)
        && (0.0..=1.0).contains(&y1)
        && (0.0..=1.0).contains(&x2)
        && (0.0..=1.0).contains(&y2)
    {
        x1 *= frame_w as f32;
        y1 *= frame_h as f32;
        x2 *= frame_w as f32;
        y2 *= frame_h as f32;
    }
    ((x1, y1), (x2, y2))
}

fn cross_2d(ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    ax * by - ay * bx
}

/// Tests whether trajectory segment `p_prev -> p_curr` intersects tripwire segment `l_start -> l_end`
/// and returns the crossing direction (`AToB` when `(L_end - L_start) x (P_curr - P_prev) > 0`, else `BToA`).
pub fn classify_segment_crossing(
    l_start: (f32, f32),
    l_end: (f32, f32),
    p_prev: (f32, f32),
    p_curr: (f32, f32),
) -> Option<LineDirection> {
    if !l_start.0.is_finite()
        || !l_start.1.is_finite()
        || !l_end.0.is_finite()
        || !l_end.1.is_finite()
        || !p_prev.0.is_finite()
        || !p_prev.1.is_finite()
        || !p_curr.0.is_finite()
        || !p_curr.1.is_finite()
    {
        return None;
    }
    let lx = l_end.0 - l_start.0;
    let ly = l_end.1 - l_start.1;
    let px = p_curr.0 - p_prev.0;
    let py = p_curr.1 - p_prev.1;
    let l_len_sq = lx * lx + ly * ly;
    let p_len_sq = px * px + py * py;
    if !l_len_sq.is_finite() || !p_len_sq.is_finite() || l_len_sq < 1e-4 || p_len_sq < 1e-4 {
        return None;
    }

    let denom = cross_2d(lx, ly, px, py);
    if !denom.is_finite()
        || denom.abs() < 1e-5
        || (denom as f64 * denom as f64) < 1e-10 * (l_len_sq as f64) * (p_len_sq as f64)
    {
        return None;
    }

    // Solve L_start + t * L = P_prev + u * P
    let dx = p_prev.0 - l_start.0;
    let dy = p_prev.1 - l_start.1;
    let t = cross_2d(dx, dy, px, py) / denom;
    let u = cross_2d(dx, dy, lx, ly) / denom;

    if t.is_finite() && u.is_finite() && (0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u) {
        if denom > 0.0 {
            Some(LineDirection::AToB)
        } else {
            Some(LineDirection::BToA)
        }
    } else {
        None
    }
}

fn association_similarity(a: &BoundingBox, b: &BoundingBox) -> f32 {
    let iou = a.iou(b);
    let (ax, ay) = a.bottom_center();
    let (bx, by) = b.bottom_center();
    let norm_dx = (ax - bx).abs() / (a.width().max(16) as f32);
    let norm_dy = (ay - by).abs() / (a.height().max(16) as f32);
    let dist = norm_dx.hypot(norm_dy);
    let dist_sim = (1.0 - dist * 0.4).max(0.0);
    let sim = iou.max(dist_sim * 0.65);
    if sim.is_finite() { sim } else { 0.0 }
}

/// Computes attribute quality score `attr.score * sqrt(attr.area_px / (frame_w * frame_h))`
/// used to select the frame where a `face` or `license_plate` is largest and sharpest.
pub fn compute_attribute_quality(score: f32, area_px: u32, frame_w: u32, frame_h: u32) -> f32 {
    if !score.is_finite() || score <= 0.0 {
        return 0.0;
    }
    let frame_area = (frame_w.saturating_mul(frame_h)).max(1) as f32;
    score * ((area_px as f32) / frame_area).sqrt()
}

pub fn is_vehicle_label(label: &str) -> bool {
    matches!(
        label,
        "car" | "truck" | "bus" | "motorcycle" | "bicycle" | "garbage_truck" | "school_bus"
    )
}

pub fn is_delivery_logo_label(label: &str) -> bool {
    !label.eq_ignore_ascii_case("face") && !label.eq_ignore_ascii_case("license_plate")
}

/// Evaluates whether a newly observed detection frame should replace the active event's snapshot
/// according to the camera's configured `SnapshotSelectionPolicy` and child attribute quality.
// Hot-path scalar and slice arguments avoid allocating an intermediate struct per frame
#[expect(clippy::too_many_arguments)]
pub fn should_replace_snapshot(
    policy: SnapshotSelectionPolicy,
    is_first_snapshot: bool,
    prev_best_score: f32,
    new_score: f32,
    prev_best_attr_quality: f32,
    new_attr_quality: f32,
    current_zones: &[String],
    entered_zones: &[String],
) -> bool {
    if is_first_snapshot {
        return true;
    }
    let safe_prev_score = if prev_best_score.is_finite() {
        prev_best_score
    } else {
        0.0
    };
    let safe_prev_attr = if prev_best_attr_quality.is_finite() {
        prev_best_attr_quality
    } else {
        0.0
    };
    let attr_improved = new_attr_quality.is_finite()
        && ((safe_prev_attr <= 0.0 && new_attr_quality > 0.0)
            || new_attr_quality > safe_prev_attr + 0.02);
    let score_improved = new_score.is_finite() && new_score > safe_prev_score + 0.05;
    let in_zone_ok = entered_zones.is_empty() || !current_zones.is_empty();

    match policy {
        SnapshotSelectionPolicy::FirstTrigger => false,
        SnapshotSelectionPolicy::BestOverall => attr_improved || score_improved,
        SnapshotSelectionPolicy::BestInZone => in_zone_ok && (attr_improved || score_improved),
        SnapshotSelectionPolicy::BestAttribute => {
            if attr_improved {
                true
            } else if safe_prev_attr > 0.0 {
                false
            } else {
                in_zone_ok && score_improved
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytetracker_tracks_across_high_and_low_score_frames_and_preserves_stationary() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 2;
        cam_cfg.detect.stationary.threshold = 10;

        let mut tracker = ByteTracker::new("front_door", 1280, 720, &cam_cfg);
        let region = BoundingBox::new(0, 0, 300, 300);

        // Frame 1: high-confidence detection (0.85)
        let d1 = DetectionResult {
            label: "person".to_string(),
            score: 0.85,
            bbox: BoundingBox::new(100, 100, 200, 300),
            region,
        };
        let (active1, _) = tracker.update(1000.0, &[d1], &[region], &cam_cfg);
        assert!(active1.is_empty());

        // Frame 2: low-confidence detection (0.55 < threshold 0.70, recovered via ByteTrack stage 2!)
        let d2 = DetectionResult {
            label: "person".to_string(),
            score: 0.55,
            bbox: BoundingBox::new(100, 100, 200, 300),
            region,
        };
        let (active2, _) = tracker.update(1000.2, std::slice::from_ref(&d2), &[region], &cam_cfg);
        assert_eq!(active2.len(), 1);
        assert_eq!(active2[0].label, "person");
        assert_eq!(active2[0].top_score, 0.85);
        assert_eq!(active2[0].provenance.area_px, 20000);
        assert_eq!(active2[0].provenance.aspect_ratio, 0.5);
        assert_eq!(active2[0].provenance.frames_tracked, 2);

        // Feed 10 motionless frames so object becomes stationary
        for i in 0..10 {
            let _ = tracker.update(
                1001.0 + i as f64,
                std::slice::from_ref(&d2),
                &[region],
                &cam_cfg,
            );
        }
        let active_stat: Vec<_> = tracker.active_tracks().cloned().collect();
        assert_eq!(active_stat.len(), 1);
        assert!(active_stat[0].stationary);

        // Next frame with NO scanned regions: stationary track must remain active!
        let (active_after_skip, _) = tracker.update(1020.0, &[], &[], &cam_cfg);
        assert_eq!(active_after_skip.len(), 1);
    }

    #[test]
    fn snapshot_selection_policies_respect_zone_and_first_trigger_rules() {
        let z_driveway = vec!["driveway".to_string()];
        let empty: Vec<String> = Vec::new();

        // FirstTrigger locks on first frame and never overwrites
        assert!(should_replace_snapshot(
            SnapshotSelectionPolicy::FirstTrigger,
            true,
            0.0,
            0.75,
            0.0,
            0.0,
            &z_driveway,
            &z_driveway
        ));
        assert!(!should_replace_snapshot(
            SnapshotSelectionPolicy::FirstTrigger,
            false,
            0.75,
            0.95,
            0.0,
            0.0,
            &z_driveway,
            &z_driveway
        ));

        // BestInZone rejects out-of-zone higher score when track previously entered a zone
        assert!(!should_replace_snapshot(
            SnapshotSelectionPolicy::BestInZone,
            false,
            0.78,
            0.92,
            0.0,
            0.0,
            &empty,
            &z_driveway
        ));
        assert!(should_replace_snapshot(
            SnapshotSelectionPolicy::BestInZone,
            false,
            0.78,
            0.92,
            0.0,
            0.0,
            &z_driveway,
            &z_driveway
        ));

        // BestOverall replaces even when out of zone if score improves
        assert!(should_replace_snapshot(
            SnapshotSelectionPolicy::BestOverall,
            false,
            0.78,
            0.92,
            0.0,
            0.0,
            &empty,
            &z_driveway
        ));
    }

    #[test]
    fn directional_tripwire_line_crossing_detects_a_to_b_b_to_a_and_non_crossing() {
        let l_start = (100.0, 200.0);
        let l_end = (500.0, 200.0);

        // Moving downward across horizontal line (y: 150 -> 250): (400, 0) x (0, 100) = +40000 => AToB
        assert_eq!(
            classify_segment_crossing(l_start, l_end, (300.0, 150.0), (300.0, 250.0)),
            Some(LineDirection::AToB)
        );

        // Moving upward across horizontal line (y: 250 -> 150): (400, 0) x (0, -100) = -40000 => BToA
        assert_eq!(
            classify_segment_crossing(l_start, l_end, (300.0, 250.0), (300.0, 150.0)),
            Some(LineDirection::BToA)
        );

        // Parallel movement above line does not intersect
        assert_eq!(
            classify_segment_crossing(l_start, l_end, (150.0, 150.0), (450.0, 150.0)),
            None
        );

        // Verify ByteTracker records crossed_lines and promotes to entered_zones
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 2;
        cam_cfg.lines.insert(
            "driveway_entry".to_string(),
            LineConfig {
                points: [100.0, 200.0, 500.0, 200.0],
                direction: LineDirection::AToB,
                objects: vec!["person".to_string()],
                ..Default::default()
            },
        );

        let mut tracker = ByteTracker::new("front_door", 640, 360, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 640, 360);
        // Frame 1: bottom_center at (300, 180)
        let d1 = DetectionResult {
            label: "person".to_string(),
            score: 0.85,
            bbox: BoundingBox::new(280, 100, 320, 180),
            region: reg,
        };
        let _ = tracker.update(1000.0, &[d1], &[reg], &cam_cfg);

        // Frame 2: bottom_center moves to (300, 230), crossing y=200 from A to B
        let d2 = DetectionResult {
            label: "person".to_string(),
            score: 0.88,
            bbox: BoundingBox::new(280, 150, 320, 230),
            region: reg,
        };
        let (active, _) = tracker.update(1000.5, &[d2], &[reg], &cam_cfg);
        assert_eq!(active.len(), 1);
        assert!(
            active[0]
                .crossed_lines
                .contains(&"driveway_entry".to_string())
        );
        assert!(
            active[0]
                .entered_zones
                .contains(&"driveway_entry".to_string())
        );
    }

    #[test]
    fn night_ir_mode_suppresses_transient_two_frame_spider_flashes() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 2;
        cam_cfg.detect.min_initialized_night = Some(4);

        let mut tracker = ByteTracker::new("front_door", 640, 360, &cam_cfg);
        tracker.set_night_mode(true);
        let reg = BoundingBox::new(0, 0, 640, 360);
        let det = DetectionResult {
            label: "person".to_string(),
            score: 0.78,
            bbox: BoundingBox::new(100, 100, 180, 260),
            region: reg,
        };

        // 2 frames (would confirm in daytime, but suppressed in IR night mode!)
        let (a1, _) = tracker.update(10.0, std::slice::from_ref(&det), &[reg], &cam_cfg);
        assert!(a1.is_empty());
        let (a2, _) = tracker.update(10.2, std::slice::from_ref(&det), &[reg], &cam_cfg);
        assert!(
            a2.is_empty(),
            "2-frame flash must be suppressed in night IR mode"
        );

        // Frame 3 & 4 confirm legitimate sustained object
        let _ = tracker.update(10.4, std::slice::from_ref(&det), &[reg], &cam_cfg);
        let (a4, _) = tracker.update(10.6, std::slice::from_ref(&det), &[reg], &cam_cfg);
        assert_eq!(a4.len(), 1);
    }

    #[test]
    fn fractional_area_bounds_and_per_zone_overrides_filter_accurately() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 1;
        // 1% of 1000x1000 = 10,000 px² minimum
        if let Some(person_f) = cam_cfg.objects.filters.get_mut("person") {
            person_f.min_area = 0.01;
            person_f.max_area = 0.50;
        }
        cam_cfg.zones.insert(
            "foreground_driveway".to_string(),
            ninjanvr_config::ZoneConfig {
                coordinates: "0.0,0.0,1.0,0.0,1.0,1.0,0.0,1.0".into(),
                objects: vec!["person".to_string()],
                inertia: 1,
                loitering_time: 0,
                min_area: Some(0.05), // 5% of frame (50,000 px²) required to enter foreground zone
                max_area: None,
                min_score: Some(0.80),
                preset: None,
                ..Default::default()
            },
        );

        let mut tracker = ByteTracker::new("front_door", 1000, 1000, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 1000, 1000);

        // 1. Box 50x50 = 2,500 px² (< 1% = 10,000 px²) -> rejected by camera fractional min_area
        let tiny_det = DetectionResult {
            label: "person".to_string(),
            score: 0.90,
            bbox: BoundingBox::new(100, 100, 150, 150),
            region: reg,
        };
        let (a_tiny, _) = tracker.update(1.0, &[tiny_det], &[reg], &cam_cfg);
        assert!(a_tiny.is_empty());

        // 2. Box 150x150 = 22,500 px² (>= 1% camera min_area, but < 5% zone min_area) -> tracked, but not in zone
        let mid_det = DetectionResult {
            label: "person".to_string(),
            score: 0.85,
            bbox: BoundingBox::new(100, 100, 250, 250),
            region: reg,
        };
        let (a_mid, _) = tracker.update(2.0, &[mid_det], &[reg], &cam_cfg);
        assert_eq!(a_mid.len(), 1);
        assert!(a_mid[0].zones.is_empty());

        // 3. Box 300x300 = 90,000 px² (>= 5% zone min_area and score 0.88 >= 0.80) -> enters zone!
        let large_det = DetectionResult {
            label: "person".to_string(),
            score: 0.88,
            bbox: BoundingBox::new(100, 100, 400, 400),
            region: reg,
        };
        let (a_large, _) = tracker.update(3.0, &[large_det], &[reg], &cam_cfg);
        assert!(
            a_large
                .iter()
                .any(|t| t.zones.contains(&"foreground_driveway".to_string()))
        );
    }

    #[test]
    fn reconfigure_regions_moves_zones_and_masks_without_dropping_tracks() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 1;
        let zone = |coords: &str| ninjanvr_config::ZoneConfig {
            coordinates: coords.into(),
            objects: vec!["person".to_string()],
            inertia: 1,
            loitering_time: 0,
            min_area: None,
            max_area: None,
            min_score: None,
            preset: None,
            ..Default::default()
        };
        cam_cfg
            .zones
            .insert("left".to_string(), zone("0.0,0.0,0.5,0.0,0.5,1.0,0.0,1.0"));
        let mut tracker = ByteTracker::new("cam", 1000, 1000, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 1000, 1000);
        let det = || DetectionResult {
            label: "person".to_string(),
            score: 0.9,
            bbox: BoundingBox::new(100, 300, 250, 600),
            region: reg,
        };

        let (first, _) = tracker.update(1.0, &[det()], &[reg], &cam_cfg);
        assert_eq!(first.len(), 1);
        assert!(first[0].zones.contains(&"left".to_string()));
        let track_id = first[0].id.clone();

        // Live edit: the zone moves to the right half and a new zone covers the person.
        cam_cfg.zones.clear();
        cam_cfg
            .zones
            .insert("right".to_string(), zone("0.5,0.0,1.0,0.0,1.0,1.0,0.5,1.0"));
        cam_cfg
            .zones
            .insert("porch".to_string(), zone("0.0,0.0,0.4,0.0,0.4,1.0,0.0,1.0"));
        tracker.reconfigure_regions(&cam_cfg);
        let (second, _) = tracker.update(1.2, &[det()], &[reg], &cam_cfg);
        assert_eq!(second.len(), 1);
        assert_eq!(
            second[0].id, track_id,
            "active track must survive a live config edit"
        );
        assert!(!second[0].zones.contains(&"left".to_string()));
        assert!(!second[0].zones.contains(&"right".to_string()));
        assert!(second[0].zones.contains(&"porch".to_string()));

        // Live edit: an object mask now covers the person's base point.
        cam_cfg.objects.mask = "0.0,0.5,0.5,0.5,0.5,0.7,0.0,0.7".into();
        tracker.reconfigure_regions(&cam_cfg);
        let fresh = DetectionResult {
            label: "person".to_string(),
            score: 0.9,
            bbox: BoundingBox::new(700, 300, 800, 600),
            region: reg,
        };
        let masked = DetectionResult {
            bbox: BoundingBox::new(100, 300, 250, 600),
            ..fresh.clone()
        };
        let (third, _) = tracker.update(1.4, &[fresh, masked], &[reg], &cam_cfg);
        assert!(
            third.iter().all(|t| t.id != track_id || t.zones.is_empty()),
            "masked detection must not refresh zone membership"
        );
        assert!(third.iter().any(|t| t.zones.contains(&"right".to_string())));
    }

    #[test]
    fn zone_loitering_time_and_inertia_hysteresis_prevent_premature_zone_entry() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 1;
        cam_cfg.zones.insert(
            "porch_inertia".to_string(),
            ninjanvr_config::ZoneConfig {
                coordinates: "0.0,0.0,0.5,0.0,0.5,1.0,0.0,1.0".into(),
                objects: vec!["person".to_string()],
                inertia: 3,
                loitering_time: 0,
                min_area: None,
                max_area: None,
                min_score: None,
                preset: None,
                ..Default::default()
            },
        );
        cam_cfg.zones.insert(
            "yard_loiter".to_string(),
            ninjanvr_config::ZoneConfig {
                coordinates: "0.5,0.0,1.0,0.0,1.0,1.0,0.5,1.0".into(),
                objects: vec!["person".to_string()],
                inertia: 1,
                loitering_time: 5,
                min_area: None,
                max_area: None,
                min_score: None,
                preset: None,
                ..Default::default()
            },
        );

        let mut tracker = ByteTracker::new("front_door", 1000, 1000, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 1000, 1000);

        // --- Part 1: Test inertia = 3 with boundary oscillation hysteresis ---
        let in_porch = DetectionResult {
            label: "person".to_string(),
            score: 0.90,
            bbox: BoundingBox::new(440, 300, 540, 500), // bottom_center = (490, 500) inside porch_inertia (x <= 500)
            region: reg,
        };
        let outside_porch = DetectionResult {
            label: "person".to_string(),
            score: 0.90,
            bbox: BoundingBox::new(460, 300, 560, 500), // bottom_center = (510, 500) just outside porch_inertia (x > 500)
            region: reg,
        };

        // Frame 1 & 2 inside porch_inertia (< 3 inertia frames) -> not yet in zone
        let (a1, _) = tracker.update(100.0, std::slice::from_ref(&in_porch), &[reg], &cam_cfg);
        assert!(a1[0].zones.is_empty());
        let (a2, _) = tracker.update(100.5, std::slice::from_ref(&in_porch), &[reg], &cam_cfg);
        assert!(a2[0].zones.is_empty());

        // Frame 3 oscillates outside porch_inertia -> resets consecutive frame counter
        let (a3, _) = tracker.update(101.0, &[outside_porch], &[reg], &cam_cfg);
        assert!(!a3[0].zones.contains(&"porch_inertia".to_string()));

        // Re-enter porch_inertia: needs 3 consecutive frames (Frames 4, 5, 6)
        let (a4, _) = tracker.update(101.5, std::slice::from_ref(&in_porch), &[reg], &cam_cfg);
        assert!(!a4[0].zones.contains(&"porch_inertia".to_string()));
        let (a5, _) = tracker.update(102.0, std::slice::from_ref(&in_porch), &[reg], &cam_cfg);
        assert!(!a5[0].zones.contains(&"porch_inertia".to_string()));
        let (a6, _) = tracker.update(102.5, std::slice::from_ref(&in_porch), &[reg], &cam_cfg);
        assert!(a6[0].zones.contains(&"porch_inertia".to_string()));
        assert!(a6[0].entered_zones.contains(&"porch_inertia".to_string()));

        // --- Part 2: Test loitering_time = 5s ---
        let mut loiter_tracker = ByteTracker::new("front_door", 1000, 1000, &cam_cfg);
        let in_yard = DetectionResult {
            label: "person".to_string(),
            score: 0.91,
            bbox: BoundingBox::new(700, 300, 800, 500), // bottom_center = (750, 500) inside yard_loiter
            region: reg,
        };

        // t=200.0s (elapsed 0.0s < 5.0s) and t=203.5s (elapsed 3.5s < 5.0s) -> not in zone
        let (l1, _) =
            loiter_tracker.update(200.0, std::slice::from_ref(&in_yard), &[reg], &cam_cfg);
        assert!(l1[0].zones.is_empty());
        let (l2, _) =
            loiter_tracker.update(203.5, std::slice::from_ref(&in_yard), &[reg], &cam_cfg);
        assert!(l2[0].zones.is_empty());

        // t=205.2s (elapsed 5.2s >= 5.0s) -> loitering threshold met, zone entry triggers!
        let (l3, _) =
            loiter_tracker.update(205.2, std::slice::from_ref(&in_yard), &[reg], &cam_cfg);
        assert!(l3[0].zones.contains(&"yard_loiter".to_string()));
        assert!(l3[0].entered_zones.contains(&"yard_loiter".to_string()));
    }

    #[test]
    fn night_mode_enforces_both_night_threshold_and_min_initialized_night() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 2;
        cam_cfg.detect.min_initialized_night = Some(3);
        if let Some(person_f) = cam_cfg.objects.filters.get_mut("person") {
            person_f.min_score = 0.50;
            person_f.threshold = 0.70;
            person_f.night_threshold = Some(0.82);
        }

        let mut tracker = ByteTracker::new("front_door", 640, 360, &cam_cfg);
        assert!(!tracker.is_night_mode());
        tracker.set_night_mode(true);
        assert!(tracker.is_night_mode());

        let reg = BoundingBox::new(0, 0, 640, 360);
        // Detection with score 0.75 (>= daytime 0.70, but < night_threshold 0.82) cannot initialize a track at night
        let marginal_det = DetectionResult {
            label: "person".to_string(),
            score: 0.75,
            bbox: BoundingBox::new(100, 100, 180, 260),
            region: reg,
        };
        for i in 0..4 {
            let (act, _) = tracker.update(
                50.0 + i as f64,
                std::slice::from_ref(&marginal_det),
                &[reg],
                &cam_cfg,
            );
            assert!(
                act.is_empty(),
                "score below night_threshold must not initialize a track in night mode"
            );
        }

        // Detection with score 0.86 (>= night_threshold 0.82) initializes after min_initialized_night (3) frames
        let strong_det = DetectionResult {
            label: "person".to_string(),
            score: 0.86,
            bbox: BoundingBox::new(100, 100, 180, 260),
            region: reg,
        };
        let (n1, _) = tracker.update(60.0, std::slice::from_ref(&strong_det), &[reg], &cam_cfg);
        assert!(n1.is_empty());
        let (n2, _) = tracker.update(60.5, std::slice::from_ref(&strong_det), &[reg], &cam_cfg);
        assert!(n2.is_empty());
        let (n3, _) = tracker.update(61.0, std::slice::from_ref(&strong_det), &[reg], &cam_cfg);
        assert_eq!(n3.len(), 1);
    }

    #[test]
    fn max_disappeared_expires_confirmed_track_into_ended_tracks_and_stationary_interval_scans() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 2;
        cam_cfg.detect.max_disappeared = 5;
        cam_cfg.detect.stationary.threshold = 10;
        cam_cfg.detect.stationary.interval = 10;

        let mut tracker = ByteTracker::new("front_door", 640, 360, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 640, 360);
        let bbox = BoundingBox::new(120, 80, 220, 280);
        let det = DetectionResult {
            label: "person".to_string(),
            score: 0.89,
            bbox,
            region: reg,
        };

        // Feed 11 frames so the track is confirmed AND marked stationary (motionless_count >= 10)
        for i in 0..11 {
            let _ = tracker.update(
                100.0 + i as f64,
                std::slice::from_ref(&det),
                &[reg],
                &cam_cfg,
            );
        }
        let active_now: Vec<_> = tracker.active_tracks().cloned().collect();
        assert_eq!(active_now.len(), 1);
        assert!(active_now[0].stationary);

        // At frame_seq == 11 (not a multiple of stationary_interval = 10) with no motion boxes:
        // non_stationary_boxes skips the stationary object
        assert!(tracker.non_stationary_boxes(&[]).is_empty());
        // When a motion box intersects the stationary track, non_stationary_boxes includes it
        assert_eq!(tracker.non_stationary_boxes(&[bbox]), vec![bbox]);

        // Advance to frame_seq == 20 (multiple of stationary_interval = 10) while keeping track stationary
        for i in 11..20 {
            let _ = tracker.update(
                100.0 + i as f64,
                std::slice::from_ref(&det),
                &[reg],
                &cam_cfg,
            );
        }
        // At frame_seq == 20, periodic forced scan returns the stationary track's bbox even with empty motion_boxes!
        assert_eq!(tracker.non_stationary_boxes(&[]), vec![bbox]);

        // Now feed max_disappeared (5) empty frames where scanned_regions covers the bbox -> still retained
        for i in 0..5 {
            let (act, ended) = tracker.update(200.0 + i as f64, &[], &[reg], &cam_cfg);
            assert!(act.is_empty());
            assert!(
                ended.is_empty(),
                "track must not expire before exceeding max_disappeared"
            );
        }

        // Frame max_disappeared + 1 (6th missed frame): track expires into ended_tracks!
        let (act_final, ended_final) = tracker.update(206.0, &[], &[reg], &cam_cfg);
        assert!(act_final.is_empty());
        assert_eq!(ended_final.len(), 1);
        assert_eq!(ended_final[0].label, "person");
    }

    #[test]
    fn classify_segment_crossing_covers_any_direction_and_non_intersecting_segments() {
        let l_start = (100.0, 200.0);
        let l_end = (500.0, 200.0);

        // Non-intersecting segment that stops short of the line (u > 1.0 needed to reach y=200)
        assert_eq!(
            classify_segment_crossing(l_start, l_end, (300.0, 100.0), (300.0, 180.0)),
            None
        );

        // Non-intersecting segment whose x-coordinate misses the [100..500] segment (t < 0.0)
        assert_eq!(
            classify_segment_crossing(l_start, l_end, (50.0, 150.0), (50.0, 250.0)),
            None
        );

        // Verify LineDirection::Any in ByteTracker triggers on both BToA and AToB crossings
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 1;
        cam_cfg.lines.insert(
            "any_tripwire".to_string(),
            LineConfig {
                points: [0.1, 0.5, 0.9, 0.5], // normalized line at y = 180 on 640x360 frame
                direction: LineDirection::Any,
                objects: vec!["person".to_string()],
                ..Default::default()
            },
        );
        let mut tracker = ByteTracker::new("front_door", 640, 360, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 640, 360);

        // Frame 1: bottom_center at (300, 240) below y=180
        let d1 = DetectionResult {
            label: "person".to_string(),
            score: 0.90,
            bbox: BoundingBox::new(280, 160, 320, 240),
            region: reg,
        };
        let _ = tracker.update(10.0, &[d1], &[reg], &cam_cfg);

        // Frame 2: bottom_center moves upward to (300, 140) (BToA crossing) -> matches LineDirection::Any
        let d2 = DetectionResult {
            label: "person".to_string(),
            score: 0.91,
            bbox: BoundingBox::new(280, 60, 320, 140),
            region: reg,
        };
        let (active, _) = tracker.update(10.5, &[d2], &[reg], &cam_cfg);
        assert_eq!(active.len(), 1);
        assert!(
            active[0]
                .crossed_lines
                .contains(&"any_tripwire".to_string())
        );
    }

    #[test]
    fn attribute_detections_bind_to_parent_track_without_spawning_standalone_tracks() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 1;
        cam_cfg.objects.track = vec!["person".to_string(), "car".to_string()];

        let mut tracker = ByteTracker::new("front_door", 1280, 720, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 1280, 720);

        let dets = vec![
            DetectionResult {
                label: "person".to_string(),
                score: 0.89,
                bbox: BoundingBox::new(100, 100, 220, 420),
                region: reg,
            },
            DetectionResult {
                label: "face".to_string(),
                score: 0.62, // below primary threshold 0.70, but attribute detections bypass threshold
                bbox: BoundingBox::new(130, 110, 190, 170),
                region: reg,
            },
            DetectionResult {
                label: "car".to_string(),
                score: 0.93,
                bbox: BoundingBox::new(400, 200, 900, 600),
                region: reg,
            },
            DetectionResult {
                label: "license_plate".to_string(),
                score: 0.86,
                bbox: BoundingBox::new(580, 480, 700, 530),
                region: reg,
            },
            DetectionResult {
                label: "correios".to_string(),
                score: 0.81,
                bbox: BoundingBox::new(500, 280, 660, 380),
                region: reg,
            },
        ];

        let (active, ended) = tracker.update(100.0, &dets, &[reg], &cam_cfg);
        assert!(ended.is_empty());
        assert_eq!(
            active.len(),
            2,
            "only person and car tracks may exist; face, license_plate, and correios must not spawn standalone tracks"
        );

        let person_trk = active.iter().find(|t| t.label == "person").unwrap();
        assert_eq!(person_trk.attributes.len(), 1);
        assert_eq!(person_trk.attributes[0].label, "face");
        assert_eq!(person_trk.attributes[0].area_px, 60 * 60);
        assert!(person_trk.best_attribute_score > 0.0);
        assert_eq!(person_trk.inferred_sub_label, None);

        let car_trk = active.iter().find(|t| t.label == "car").unwrap();
        assert_eq!(car_trk.attributes.len(), 2);
        assert!(
            car_trk
                .attributes
                .iter()
                .any(|a| a.label == "license_plate")
        );
        assert!(car_trk.attributes.iter().any(|a| a.label == "correios"));
        assert_eq!(car_trk.inferred_sub_label.as_deref(), Some("correios"));
        assert!(car_trk.best_attribute_score > 0.0);
    }

    #[test]
    fn orphan_attributes_outside_parent_boxes_are_discarded() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 1;
        cam_cfg.objects.track = vec!["person".to_string(), "car".to_string()];

        let mut tracker = ByteTracker::new("driveway", 1280, 720, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 1280, 720);

        // 1. Orphan face and license_plate with no parent tracks at all
        let orphan_only = vec![
            DetectionResult {
                label: "face".to_string(),
                score: 0.91,
                bbox: BoundingBox::new(50, 50, 100, 100),
                region: reg,
            },
            DetectionResult {
                label: "license_plate".to_string(),
                score: 0.94,
                bbox: BoundingBox::new(800, 500, 900, 540),
                region: reg,
            },
        ];
        let (active_empty, _) = tracker.update(10.0, &orphan_only, &[reg], &cam_cfg);
        assert!(
            active_empty.is_empty(),
            "orphan attributes must never create standalone tracks"
        );

        // 2. Distant face/plate outside the person/car bounding boxes (containment < 0.60)
        let distant = vec![
            DetectionResult {
                label: "person".to_string(),
                score: 0.88,
                bbox: BoundingBox::new(100, 100, 200, 350),
                region: reg,
            },
            DetectionResult {
                label: "face".to_string(),
                score: 0.92,
                bbox: BoundingBox::new(600, 100, 650, 150), // far away from person
                region: reg,
            },
            DetectionResult {
                label: "car".to_string(),
                score: 0.90,
                bbox: BoundingBox::new(300, 200, 600, 450),
                region: reg,
            },
            DetectionResult {
                label: "license_plate".to_string(),
                score: 0.89,
                bbox: BoundingBox::new(900, 500, 980, 540), // far away from car
                region: reg,
            },
        ];
        let (active, _) = tracker.update(11.0, &distant, &[reg], &cam_cfg);
        assert_eq!(active.len(), 2);
        for trk in &active {
            assert!(
                trk.attributes.is_empty(),
                "uncontained attributes must not attach to distant tracks"
            );
            assert_eq!(trk.best_attribute_score, 0.0);
        }
    }

    #[test]
    fn should_replace_snapshot_upgrades_when_attribute_quality_improves() {
        let z_porch = vec!["porch".to_string()];
        let q1 = compute_attribute_quality(0.75, 30 * 30, 1280, 720);
        let q2 = compute_attribute_quality(0.92, 90 * 90, 1280, 720);
        assert!(q2 > q1 + 0.02);

        // Gaining first face/license_plate replaces snapshot even when parent score drops (0.82 -> 0.81)
        assert!(should_replace_snapshot(
            SnapshotSelectionPolicy::BestAttribute,
            false,
            0.82,
            0.81,
            0.0,
            q1,
            &z_porch,
            &z_porch,
        ));

        // Improving face quality (q1 -> q2) replaces snapshot even when parent score is flat (0.82 -> 0.81)
        assert!(should_replace_snapshot(
            SnapshotSelectionPolicy::BestAttribute,
            false,
            0.82,
            0.81,
            q1,
            q2,
            &z_porch,
            &z_porch,
        ));
        assert!(should_replace_snapshot(
            SnapshotSelectionPolicy::BestInZone,
            false,
            0.82,
            0.81,
            q1,
            q2,
            &z_porch,
            &z_porch,
        ));

        // Under BestAttribute, once a face snapshot is captured (prev_best_attr_quality = q2 > 0),
        // a later frame with no face (0.0) does NOT overwrite the snapshot even if parent score rises
        assert!(!should_replace_snapshot(
            SnapshotSelectionPolicy::BestAttribute,
            false,
            0.82,
            0.95,
            q2,
            0.0,
            &z_porch,
            &z_porch,
        ));
    }

    #[test]
    fn preset_bound_zones_only_evaluate_when_camera_is_at_matching_preset() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 1;
        cam_cfg.zones.insert(
            "driveway_zone".to_string(),
            ninjanvr_config::ZoneConfig {
                coordinates: "0.0,0.0,1.0,0.0,1.0,1.0,0.0,1.0".into(),
                objects: vec!["person".to_string()],
                inertia: 1,
                loitering_time: 0,
                min_area: None,
                max_area: None,
                min_score: None,
                preset: Some("driveway".to_string()),
                ..Default::default()
            },
        );
        cam_cfg.zones.insert(
            "entry_gate_zone".to_string(),
            ninjanvr_config::ZoneConfig {
                coordinates: "0.0,0.0,1.0,0.0,1.0,1.0,0.0,1.0".into(),
                objects: vec!["person".to_string()],
                inertia: 1,
                loitering_time: 0,
                min_area: None,
                max_area: None,
                min_score: None,
                preset: Some("entry_gate".to_string()),
                ..Default::default()
            },
        );
        cam_cfg.zones.insert(
            "global_perimeter".to_string(),
            ninjanvr_config::ZoneConfig {
                coordinates: "0.0,0.0,1.0,0.0,1.0,1.0,0.0,1.0".into(),
                objects: vec!["person".to_string()],
                inertia: 1,
                loitering_time: 0,
                min_area: None,
                max_area: None,
                min_score: None,
                preset: None,
                ..Default::default()
            },
        );

        let mut tracker = ByteTracker::new("ptz_cam", 1000, 1000, &cam_cfg);
        assert_eq!(tracker.active_preset(), None);

        let reg = BoundingBox::new(0, 0, 1000, 1000);
        let det = DetectionResult {
            label: "person".to_string(),
            score: 0.90,
            bbox: BoundingBox::new(200, 200, 350, 500),
            region: reg,
        };

        // 1. Camera moving (active_preset == None): preset-bound zones are suspended, global zone evaluates
        let (a_moving, _) = tracker.update(10.0, std::slice::from_ref(&det), &[reg], &cam_cfg);
        assert_eq!(a_moving.len(), 1);
        assert!(a_moving[0].zones.contains(&"global_perimeter".to_string()));
        assert!(!a_moving[0].zones.contains(&"driveway_zone".to_string()));
        assert!(!a_moving[0].zones.contains(&"entry_gate_zone".to_string()));

        // 2. Camera parked at "patio" (non-matching preset): preset-bound zones remain suspended, global zone evaluates
        tracker.set_active_preset(Some("patio"));
        assert_eq!(tracker.active_preset(), Some("patio"));
        let (a_patio, _) = tracker.update(11.0, std::slice::from_ref(&det), &[reg], &cam_cfg);
        assert!(a_patio[0].zones.contains(&"global_perimeter".to_string()));
        assert!(!a_patio[0].zones.contains(&"driveway_zone".to_string()));
        assert!(!a_patio[0].zones.contains(&"entry_gate_zone".to_string()));

        // 3. Camera parked at "driveway": driveway_zone + global_perimeter trigger, entry_gate_zone stays suspended
        tracker.set_active_preset(Some("driveway"));
        assert_eq!(tracker.active_preset(), Some("driveway"));
        let (a_driveway, _) = tracker.update(12.0, std::slice::from_ref(&det), &[reg], &cam_cfg);
        assert!(
            a_driveway[0]
                .zones
                .contains(&"global_perimeter".to_string())
        );
        assert!(a_driveway[0].zones.contains(&"driveway_zone".to_string()));
        assert!(!a_driveway[0].zones.contains(&"entry_gate_zone".to_string()));

        // 4. Camera moves away via ContinuousMove/RelativeMove -> clear active_preset to None
        tracker.set_active_preset(None);
        assert_eq!(tracker.active_preset(), None);
        let (a_cleared, _) = tracker.update(13.0, std::slice::from_ref(&det), &[reg], &cam_cfg);
        assert!(a_cleared[0].zones.contains(&"global_perimeter".to_string()));
        assert!(!a_cleared[0].zones.contains(&"driveway_zone".to_string()));

        // 5. GotoPreset completes to "entry_gate" -> entry_gate_zone + global_perimeter trigger
        tracker.set_active_preset(Some("entry_gate"));
        assert_eq!(tracker.active_preset(), Some("entry_gate"));
        let (a_gate, _) = tracker.update(14.0, std::slice::from_ref(&det), &[reg], &cam_cfg);
        assert!(a_gate[0].zones.contains(&"global_perimeter".to_string()));
        assert!(a_gate[0].zones.contains(&"entry_gate_zone".to_string()));
        assert!(!a_gate[0].zones.contains(&"driveway_zone".to_string()));
    }

    #[test]
    fn attribute_quality_and_label_classifiers_handle_vehicles_carriers_and_zero_frames() {
        for v in [
            "car",
            "truck",
            "bus",
            "motorcycle",
            "bicycle",
            "garbage_truck",
            "school_bus",
        ] {
            assert!(is_vehicle_label(v), "{v} must be classified as vehicle");
        }
        for non_v in ["person", "dog", "cat", "face", "license_plate"] {
            assert!(
                !is_vehicle_label(non_v),
                "{non_v} must not be classified as vehicle"
            );
        }

        assert!(!is_delivery_logo_label("face"));
        assert!(!is_delivery_logo_label("FACE"));
        assert!(!is_delivery_logo_label("license_plate"));
        assert!(!is_delivery_logo_label("License_Plate"));
        assert!(is_delivery_logo_label("correios"));
        assert!(is_delivery_logo_label("amazon"));
        assert!(is_delivery_logo_label("mercadolivre"));

        // compute_attribute_quality = score * sqrt(area_px / (frame_w * frame_h))
        // For 100x100 = 10_000 px inside 1000x1000 = 1_000_000 px -> sqrt(0.01) = 0.1
        let q = compute_attribute_quality(0.80, 10_000, 1000, 1000);
        assert!((q - 0.08).abs() < 1e-5);

        // Division-by-zero guard when frame_w or frame_h is 0
        let q_zero = compute_attribute_quality(0.90, 100, 0, 0);
        assert!(q_zero.is_finite() && q_zero > 0.0);
    }

    #[test]
    fn byte_tracker_enforces_object_masks_aspect_ratio_filters_and_duplicate_suppression() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.width = 1000;
        cam_cfg.detect.height = 1000;
        cam_cfg.detect.min_initialized = 1;
        cam_cfg.objects.track = vec!["person".to_string()];
        // Global object mask covering the top-left quadrant [0,0 -> 500,500]
        cam_cfg.objects.mask = "0.0,0.0,0.5,0.0,0.5,0.5,0.0,0.5".into();

        let filter = ninjanvr_config::ObjectFilterConfig {
            min_ratio: 0.3,
            max_ratio: 1.2,
            ..Default::default()
        };
        cam_cfg.objects.filters.insert("person".to_string(), filter);

        let mut tracker = ByteTracker::new("mask_cam", 1000, 1000, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 1000, 1000);

        // 1. Detection whose bottom_center (250, 400) is inside the object mask -> suppressed
        let masked_det = DetectionResult {
            label: "person".to_string(),
            score: 0.92,
            bbox: BoundingBox::new(200, 200, 300, 400),
            region: reg,
        };
        // 2. Detection with wide aspect ratio (width 400 / height 100 = 4.0 > max_ratio 1.2) -> suppressed
        let wide_det = DetectionResult {
            label: "person".to_string(),
            score: 0.92,
            bbox: BoundingBox::new(500, 600, 900, 700),
            region: reg,
        };
        // 3. Valid detection outside mask with ratio 100 / 200 = 0.5 and duplicate overlapping box
        let valid_det = DetectionResult {
            label: "person".to_string(),
            score: 0.91,
            bbox: BoundingBox::new(600, 600, 700, 800),
            region: reg,
        };
        let duplicate_det = DetectionResult {
            label: "person".to_string(),
            score: 0.86,
            bbox: BoundingBox::new(605, 605, 695, 795),
            region: reg,
        };

        let (active, ended) = tracker.update(
            1.0,
            &[masked_det, wide_det, valid_det, duplicate_det],
            &[reg],
            &cam_cfg,
        );
        assert!(ended.is_empty());
        assert_eq!(
            active.len(),
            1,
            "only the unmasked, valid-ratio primary detection should initialize a track"
        );
        assert_eq!(active[0].bbox, BoundingBox::new(600, 600, 700, 800));
    }

    #[test]
    fn occluded_moving_track_preserves_velocity_without_false_motionless_and_attaches_person_courier_logo()
     {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.width = 1000;
        cam_cfg.detect.height = 1000;
        cam_cfg.detect.min_initialized = 1;
        cam_cfg.objects.track = vec!["person".to_string()];

        let mut tracker = ByteTracker::new("street", 1000, 1000, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 1000, 1000);

        // Frame 1: person at x=100..200
        let det1 = DetectionResult {
            label: "person".to_string(),
            score: 0.90,
            bbox: BoundingBox::new(100, 300, 200, 600),
            region: reg,
        };
        let (a1, _) = tracker.update(1.0, &[det1], &[reg], &cam_cfg);
        assert_eq!(a1.len(), 1);

        // Frame 2: person moves +40px right to x=140..240 -> establishes positive vx
        let det2 = DetectionResult {
            label: "person".to_string(),
            score: 0.91,
            bbox: BoundingBox::new(140, 300, 240, 600),
            region: reg,
        };
        let (a2, _) = tracker.update(2.0, &[det2], &[reg], &cam_cfg);
        let vx_after_f2 = a2[0].vx;
        assert!(vx_after_f2 > 10.0);
        assert_eq!(a2[0].motionless_count, 0);

        // Frame 3: brief 1-frame occlusion (no detections inside scanned region)
        let _ = tracker.update(3.0, &[], &[reg], &cam_cfg);

        // Frame 4: person re-detected moving right at x=180..280 along with a "correios" courier logo inside person bbox
        let det4_person = DetectionResult {
            label: "person".to_string(),
            score: 0.92,
            bbox: BoundingBox::new(180, 300, 280, 600),
            region: reg,
        };
        let det4_logo = DetectionResult {
            label: "correios".to_string(),
            score: 0.85,
            bbox: BoundingBox::new(200, 380, 260, 440),
            region: reg,
        };
        let (a4, _) = tracker.update(4.0, &[det4_person, det4_logo], &[reg], &cam_cfg);
        assert_eq!(a4.len(), 1);
        assert!(
            a4[0].vx >= vx_after_f2,
            "velocity must not decay when re-matching an occluded moving track (vx={} vs {})",
            a4[0].vx,
            vx_after_f2
        );
        assert_eq!(
            a4[0].motionless_count, 0,
            "moving track re-matched at predicted position must not increment motionless_count"
        );
        assert_eq!(a4[0].inferred_sub_label.as_deref(), Some("correios"));
    }

    #[test]
    fn regression_nan_scores_degenerate_boxes_and_non_finite_tripwires_are_rejected() {
        // 1. Non-finite and overflowing tripwire coordinates must never return a false crossing
        assert_eq!(
            classify_segment_crossing(
                (100.0, 200.0),
                (f32::INFINITY, 200.0),
                (300.0, 150.0),
                (300.0, 250.0)
            ),
            None
        );
        assert_eq!(
            classify_segment_crossing(
                (100.0, 200.0),
                (500.0, 200.0),
                (300.0, f32::NAN),
                (300.0, 250.0)
            ),
            None
        );
        // Zero-length degenerate tripwire segment must return None
        assert_eq!(
            classify_segment_crossing(
                (300.0, 200.0),
                (300.0001, 200.0),
                (300.0, 100.0),
                (300.0, 300.0)
            ),
            None
        );

        // 2. NaN-score detections and oversized false-positive attribute boxes must not poison tracks
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 1;
        cam_cfg.objects.track = vec!["person".to_string()];

        let mut tracker = ByteTracker::new("cam", 1000, 1000, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 1000, 1000);
        let valid_person = DetectionResult {
            label: "person".to_string(),
            score: 0.90,
            bbox: BoundingBox::new(200, 200, 300, 400),
            region: reg,
        };
        let (a1, _) = tracker.update(10.0, &[valid_person], &[reg], &cam_cfg);
        assert_eq!(a1.len(), 1);

        let nan_person = DetectionResult {
            label: "person".to_string(),
            score: f32::NAN,
            bbox: BoundingBox::new(200, 200, 300, 400),
            region: reg,
        };
        let oversized_face = DetectionResult {
            label: "face".to_string(),
            score: 0.95,
            bbox: BoundingBox::new(50, 50, 800, 800), // larger than parent person bbox!
            region: reg,
        };
        let (a2, _) = tracker.update(11.0, &[nan_person, oversized_face], &[], &cam_cfg);
        for trk in &a2 {
            assert!(trk.score.is_finite());
            assert!(
                trk.attributes.is_empty(),
                "oversized attribute must not bind to smaller parent"
            );
        }
    }

    #[test]
    fn regression_global_stage1_similarity_ranking_prevents_track_hijacking() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 1;
        cam_cfg.objects.track = vec!["person".to_string()];

        let mut tracker = ByteTracker::new("cam", 1000, 1000, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 1000, 1000);

        // Initialize Track A at x=100..200 and Track B at x=190..290
        let init_a = DetectionResult {
            label: "person".to_string(),
            score: 0.90,
            bbox: BoundingBox::new(100, 100, 200, 300),
            region: reg,
        };
        let init_b = DetectionResult {
            label: "person".to_string(),
            score: 0.90,
            bbox: BoundingBox::new(190, 100, 290, 300),
            region: reg,
        };
        let (active_init, _) = tracker.update(1.0, &[init_a, init_b], &[reg], &cam_cfg);
        assert_eq!(active_init.len(), 2);
        let id_a = active_init
            .iter()
            .find(|t| t.bbox.x_min == 100)
            .unwrap()
            .id
            .clone();
        let id_b = active_init
            .iter()
            .find(|t| t.bbox.x_min == 190)
            .unwrap()
            .id
            .clone();

        // Next frame: Det 0 (higher score, at x=135..235) is slightly closer to A than B,
        // while Det 1 (slightly lower score, at x=100..200) is an exact 1.0 IoU match for A.
        // Global similarity ranking must pair Det 1 -> Track A and Det 0 -> Track B!
        let det_shifted = DetectionResult {
            label: "person".to_string(),
            score: 0.95,
            bbox: BoundingBox::new(135, 100, 235, 300),
            region: reg,
        };
        let det_exact_a = DetectionResult {
            label: "person".to_string(),
            score: 0.88,
            bbox: BoundingBox::new(100, 100, 200, 300),
            region: reg,
        };
        let (active_next, _) = tracker.update(2.0, &[det_shifted, det_exact_a], &[reg], &cam_cfg);
        assert_eq!(active_next.len(), 2);
        let trk_a = active_next
            .iter()
            .find(|t| t.id == id_a)
            .expect("Track A preserved");
        let trk_b = active_next
            .iter()
            .find(|t| t.id == id_b)
            .expect("Track B preserved");
        assert_eq!(trk_a.bbox, BoundingBox::new(100, 100, 200, 300));
        assert_eq!(trk_b.bbox, BoundingBox::new(135, 100, 235, 300));
    }

    #[test]
    fn regression_unscanned_stationary_track_eventually_expires_and_prevents_memory_leak() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 2;
        cam_cfg.detect.max_disappeared = 5;
        cam_cfg.detect.stationary.threshold = 10;
        cam_cfg.detect.stationary.interval = 10;

        let mut tracker = ByteTracker::new("cam", 640, 360, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 640, 360);
        let det = DetectionResult {
            label: "person".to_string(),
            score: 0.90,
            bbox: BoundingBox::new(100, 100, 200, 250),
            region: reg,
        };
        for i in 0..11 {
            let _ = tracker.update(
                10.0 + i as f64,
                std::slice::from_ref(&det),
                &[reg],
                &cam_cfg,
            );
        }
        assert_eq!(tracker.active_tracks().count(), 1);

        // Feed > 120 unscanned frames: stationary track must eventually expire instead of leaking forever
        let mut expired = false;
        for i in 0..130 {
            let (_, ended) = tracker.update(100.0 + i as f64, &[], &[], &cam_cfg);
            if !ended.is_empty() {
                expired = true;
                break;
            }
        }
        assert!(
            expired,
            "unscanned stationary track must expire after bounded interval"
        );
        assert_eq!(tracker.active_tracks().count(), 0);
    }

    #[test]
    fn regression_bottom_edge_detections_respect_masks_zones_and_snapshot_nan_guards() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 1;
        cam_cfg.objects.track = vec!["person".to_string(), "car".to_string()];
        // Per-label mask for "car" covering the bottom half [0, 0.5]..[1.0, 1.0]
        let car_filter = ninjanvr_config::ObjectFilterConfig {
            mask: "0.0,0.5,1.0,0.5,1.0,1.0,0.0,1.0".into(),
            ..Default::default()
        };
        cam_cfg
            .objects
            .filters
            .insert("car".to_string(), car_filter);
        cam_cfg.zones.insert(
            "bottom_zone".to_string(),
            ninjanvr_config::ZoneConfig {
                coordinates: "0.0,0.5,1.0,0.5,1.0,1.0,0.0,1.0".into(),
                objects: vec!["person".to_string()],
                inertia: 1,
                loitering_time: 0,
                min_area: None,
                max_area: None,
                min_score: None,
                preset: None,
                ..Default::default()
            },
        );

        let mut tracker = ByteTracker::new("cam", 1000, 1000, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 1000, 1000);

        // 1. Car touching or slightly protruding past bottom edge (y_max = 1005 -> clamped to 1000)
        // must still be suppressed by the bottom-half object mask instead of bypassing it
        let edge_car = DetectionResult {
            label: "car".to_string(),
            score: 0.92,
            bbox: BoundingBox::new(300, 750, 600, 1005),
            region: reg,
        };
        // 2. Person touching bottom edge (y_max = 1000) must still enter bottom_zone and keep norm_bbox <= 1.0
        let edge_person = DetectionResult {
            label: "person".to_string(),
            score: 0.91,
            bbox: BoundingBox::new(100, 700, 220, 1005),
            region: reg,
        };
        // 3. Completely out-of-frame detection (x_min >= frame_w) is rejected
        let oob_person = DetectionResult {
            label: "person".to_string(),
            score: 0.95,
            bbox: BoundingBox::new(1200, 1200, 1400, 1500),
            region: reg,
        };

        let (active, _) =
            tracker.update(1.0, &[edge_car, edge_person, oob_person], &[reg], &cam_cfg);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].label, "person");
        assert_eq!(active[0].bbox, BoundingBox::new(100, 700, 220, 1000));
        assert!(active[0].zones.contains(&"bottom_zone".to_string()));
        assert!(
            active[0]
                .provenance
                .norm_bbox
                .iter()
                .all(|&v| (0.0..=1.0).contains(&v))
        );
        assert!(active[0].provenance.area_pct <= 100.0);

        // Snapshot replacement rejects non-finite new scores and recovers from NaN previous scores
        let z = vec!["bottom_zone".to_string()];
        assert!(!should_replace_snapshot(
            SnapshotSelectionPolicy::BestOverall,
            false,
            0.80,
            f32::INFINITY,
            0.0,
            f32::INFINITY,
            &z,
            &z,
        ));
        assert!(should_replace_snapshot(
            SnapshotSelectionPolicy::BestOverall,
            false,
            f32::NAN,
            0.85,
            f32::NAN,
            0.0,
            &z,
            &z,
        ));
    }

    #[test]
    fn regression_pre_nms_filtering_eviction_protection_day_to_night_confirmation_and_nan_loiter() {
        let mut cam_cfg = CameraConfig::default();
        cam_cfg.detect.min_initialized = 2;
        cam_cfg.detect.min_initialized_night = Some(4);
        cam_cfg.detect.max_disappeared = 5;
        cam_cfg.objects.track = vec!["person".to_string(), "car".to_string()];
        // Global mask in top-left [0..500, 0..500]
        cam_cfg.objects.mask = "0.0,0.0,0.5,0.0,0.5,0.5,0.0,0.5".into();
        let person_filter = ninjanvr_config::ObjectFilterConfig {
            max_area: 100_000.0,
            ..Default::default()
        };
        cam_cfg
            .objects
            .filters
            .insert("person".to_string(), person_filter);
        cam_cfg.zones.insert(
            "yard_loiter".to_string(),
            ninjanvr_config::ZoneConfig {
                coordinates: "0.0,0.5,1.0,0.5,1.0,1.0,0.0,1.0".into(),
                objects: vec!["person".to_string()],
                inertia: 1,
                loitering_time: 10,
                min_area: None,
                max_area: Some(0.0), // 0.0 must be treated as unset, not 1px max
                min_score: None,
                preset: None,
                ..Default::default()
            },
        );

        let mut tracker = ByteTracker::new("cam", 1000, 1000, &cam_cfg);
        let reg = BoundingBox::new(0, 0, 1000, 1000);

        // 1. Oversized false-positive detection (800x800 = 640k > max_area 100k, higher score 0.99)
        // must be filtered BEFORE NMS so it cannot suppress a valid overlapping person (100x200 = 20k, score 0.85)!
        let oversized_fp = DetectionResult {
            label: "person".to_string(),
            score: 0.99,
            bbox: BoundingBox::new(100, 500, 900, 990),
            region: reg,
        };
        let valid_person = DetectionResult {
            label: "person".to_string(),
            score: 0.85,
            bbox: BoundingBox::new(200, 600, 300, 800),
            region: reg,
        };
        let _ = tracker.update(
            100.0,
            &[oversized_fp.clone(), valid_person.clone()],
            &[reg],
            &cam_cfg,
        );
        let (active_f2, _) = tracker.update(
            101.0,
            &[oversized_fp, valid_person.clone()],
            &[reg],
            &cam_cfg,
        );
        assert_eq!(
            active_f2.len(),
            1,
            "oversized filtered-out box must not suppress valid overlapping detection during NMS"
        );

        // 2. NaN frame_time mid-loiter must not reset first_entered_ts to 0.0 and prematurely trigger loitering zone
        let (active_nan, _) = tracker.update(
            f64::NAN,
            std::slice::from_ref(&valid_person),
            &[reg],
            &cam_cfg,
        );
        assert_eq!(active_nan.len(), 1);
        assert!(active_nan[0].zones.is_empty());

        let (active_before_loiter, _) =
            tracker.update(104.0, std::slice::from_ref(&valid_person), &[reg], &cam_cfg);
        assert!(
            active_before_loiter[0].zones.is_empty(),
            "elapsed 4s < 10s loitering_time must not trigger zone after intervening NaN timestamp"
        );

        let (active_after_loiter, _) =
            tracker.update(111.0, std::slice::from_ref(&valid_person), &[reg], &cam_cfg);
        assert!(
            active_after_loiter[0]
                .zones
                .contains(&"yard_loiter".to_string()),
            "zone with max_area=Some(0.0) must trigger once 10s loitering_time elapses"
        );

        // 3. Track confirmed in daytime (frames_tracked == 2 >= min_initialized 2, < min_initialized_night 4)
        // must remain confirmed and emit into ended_tracks even if camera switches to night mode!
        let mut dn_tracker = ByteTracker::new("cam_dn", 1000, 1000, &cam_cfg);
        let _ = dn_tracker.update(10.0, std::slice::from_ref(&valid_person), &[reg], &cam_cfg);
        let (dn_active, _) =
            dn_tracker.update(11.0, std::slice::from_ref(&valid_person), &[reg], &cam_cfg);
        assert_eq!(dn_active.len(), 1);
        dn_tracker.set_night_mode(true);
        let mut dn_ended = Vec::new();
        for i in 0..7 {
            let (_, ended) = dn_tracker.update(20.0 + i as f64, &[], &[reg], &cam_cfg);
            dn_ended.extend(ended);
        }
        assert_eq!(
            dn_ended.len(),
            1,
            "daytime-confirmed track must emit into ended_tracks after switching to night mode"
        );

        // 4. Highest-scoring delivery logo wins in same frame, and active confirmed tracks resist MAX_ACTIVE_TRACKS eviction
        let mut logo_cfg = CameraConfig::default();
        logo_cfg.detect.min_initialized = 1;
        logo_cfg.objects.track = vec!["car".to_string(), "person".to_string()];
        let mut logo_tracker = ByteTracker::new("cam_logo", 2000, 2000, &logo_cfg);
        let car_det = DetectionResult {
            label: "car".to_string(),
            score: 0.95,
            bbox: BoundingBox::new(100, 100, 500, 400),
            region: reg,
        };
        let logo_high = DetectionResult {
            label: "correios".to_string(),
            score: 0.94,
            bbox: BoundingBox::new(150, 150, 250, 220),
            region: reg,
        };
        let logo_low = DetectionResult {
            label: "dhl".to_string(),
            score: 0.55,
            bbox: BoundingBox::new(300, 150, 400, 220),
            region: reg,
        };
        let (logo_active, _) = logo_tracker.update(
            1.0,
            &[car_det.clone(), logo_high, logo_low],
            &[reg],
            &logo_cfg,
        );
        assert_eq!(logo_active.len(), 1);
        assert_eq!(
            logo_active[0].inferred_sub_label.as_deref(),
            Some("correios"),
            "higher-scoring delivery logo must not be overwritten by lower-scoring logo in same frame"
        );
        let car_id = logo_active[0].id.clone();

        // Flood 260 non-overlapping tentative detections on frame 2 while keeping car_det matched:
        // car_id must not be evicted!
        let mut flood = vec![car_det];
        for i in 0..260u32 {
            let gx = (i % 20) * 80 + 10;
            let gy = (i / 20) * 80 + 500;
            flood.push(DetectionResult {
                label: "person".to_string(),
                score: 0.80,
                bbox: BoundingBox::new(gx, gy, gx + 30, gy + 60),
                region: reg,
            });
        }
        let (flood_active, flood_ended) = logo_tracker.update(2.0, &flood, &[reg], &logo_cfg);
        assert!(
            flood_active.iter().any(|t| t.id == car_id),
            "currently-matched confirmed track must not be evicted by flood of new detections"
        );
        assert!(!flood_ended.iter().any(|t| t.id == car_id));

        // 5. Attribute flood per parent track is capped at MAX_ATTRIBUTES_PER_TRACK (32)
        let mut attr_flood = vec![DetectionResult {
            label: "car".to_string(),
            score: 0.96,
            bbox: BoundingBox::new(100, 100, 900, 900),
            region: reg,
        }];
        for i in 0..80u32 {
            let ax = 120 + (i % 10) * 60;
            let ay = 120 + (i / 10) * 60;
            attr_flood.push(DetectionResult {
                label: "license_plate".to_string(),
                score: 0.90,
                bbox: BoundingBox::new(ax, ay, ax + 40, ay + 20),
                region: reg,
            });
        }
        let (attr_active, _) = logo_tracker.update(3.0, &attr_flood, &[reg], &logo_cfg);
        let car_trk = attr_active.iter().find(|t| t.id == car_id).unwrap();
        assert_eq!(car_trk.attributes.len(), MAX_ATTRIBUTES_PER_TRACK);
    }
}
