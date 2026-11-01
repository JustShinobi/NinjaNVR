use std::collections::BTreeMap;

use ninjanvr_config::CameraConfig;
use ninjanvr_infer::{DetectionResult, nms_detections};
use ninjanvr_vision::{BoundingBox, PolygonMask};
use serde::{Deserialize, Serialize};

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
    pub has_snapshot: bool,
    pub vx: f32,
    pub vy: f32,
}

/// Multi-Object Tracker implementing 2-stage **ByteTrack** association
/// (high-confidence pass + low-confidence occlusion recovery pass),
/// constant-velocity Kalman state estimation, Stationary object classification,
/// and polygonal Zone / Object Mask evaluation.
pub struct ByteTracker {
    camera_name: String,
    frame_w: u32,
    frame_h: u32,
    min_initialized: u32,
    max_disappeared: u32,
    stationary_threshold: u32,
    stationary_interval: u32,
    global_object_mask: PolygonMask,
    label_masks: BTreeMap<String, PolygonMask>,
    zones: BTreeMap<String, (PolygonMask, Vec<String>)>,
    tracks: BTreeMap<String, TrackedObject>,
    frame_seq: u64,
}

impl ByteTracker {
    pub fn new(
        camera_name: impl Into<String>,
        frame_w: u32,
        frame_h: u32,
        cfg: &CameraConfig,
    ) -> Self {
        let global_object_mask =
            PolygonMask::from_config_value(&cfg.objects.mask, frame_w, frame_h);
        let mut label_masks = BTreeMap::new();
        for (label, f) in &cfg.objects.filters {
            let m = PolygonMask::from_config_value(&f.mask, frame_w, frame_h);
            if !m.is_empty() {
                label_masks.insert(label.clone(), m);
            }
        }

        let mut zones = BTreeMap::new();
        for (zname, zcfg) in &cfg.zones {
            let poly = PolygonMask::from_config_value(&zcfg.coordinates, frame_w, frame_h);
            if !poly.is_empty() {
                zones.insert(zname.clone(), (poly, zcfg.objects.clone()));
            }
        }

        Self {
            camera_name: camera_name.into(),
            frame_w: frame_w.max(1),
            frame_h: frame_h.max(1),
            min_initialized: cfg.detect.min_initialized.max(1),
            max_disappeared: cfg.detect.max_disappeared.max(5),
            stationary_threshold: cfg.detect.stationary.threshold.max(10),
            stationary_interval: cfg.detect.stationary.interval.max(10),
            global_object_mask,
            label_masks,
            zones,
            tracks: BTreeMap::new(),
            frame_seq: 0,
        }
    }

    pub fn active_tracks(&self) -> impl Iterator<Item = &TrackedObject> {
        self.tracks
            .values()
            .filter(|t| t.frames_tracked >= self.min_initialized && t.disappeared_frames == 0)
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
                        || motion_boxes.iter().any(|mb| mb.intersects(&t.bbox)))
            })
            .map(|t| t.bbox)
            .collect()
    }

    fn is_detection_masked_or_filtered(&self, det: &DetectionResult, cfg: &CameraConfig) -> bool {
        if !cfg.objects.track.iter().any(|l| l == &det.label) {
            return true;
        }
        let (bx, by) = det.bbox.bottom_center();
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
            if area < f.min_area || area > f.max_area {
                return true;
            }
            let ratio = (det.bbox.width() as f32) / (det.bbox.height().max(1) as f32);
            if ratio < f.min_ratio || ratio > f.max_ratio {
                return true;
            }
            if det.score < f.min_score {
                return true;
            }
        }
        false
    }

    fn evaluate_zones(&self, label: &str, bbox: &BoundingBox) -> Vec<String> {
        let (bx, by) = bbox.bottom_center();
        let mut matched = Vec::new();
        for (zname, (poly, allowed_objs)) in &self.zones {
            if (!allowed_objs.is_empty() && !allowed_objs.iter().any(|o| o == label))
                || !poly.contains_point(bx, by)
            {
                continue;
            }
            matched.push(zname.clone());
        }
        matched
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
        self.frame_seq += 1;

        let deduplicated = nms_detections(raw_detections.to_vec(), 0.55);

        // 1. Predict next state for existing tracks using constant-velocity model
        for trk in self.tracks.values_mut() {
            if trk.disappeared_frames > 0 && (trk.vx.abs() > 0.5 || trk.vy.abs() > 0.5) {
                let w = trk.bbox.width().min(self.frame_w);
                let h = trk.bbox.height().min(self.frame_h);
                let max_x = self.frame_w.saturating_sub(w) as f32;
                let max_y = self.frame_h.saturating_sub(h) as f32;
                let nx1 = ((trk.bbox.x_min as f32) + trk.vx).clamp(0.0, max_x) as u32;
                let ny1 = ((trk.bbox.y_min as f32) + trk.vy).clamp(0.0, max_y) as u32;
                trk.bbox = BoundingBox::new(nx1, ny1, nx1 + w, ny1 + h);
            }
        }

        // 2. Partition detections into High-Score and Low-Score
        let mut high_dets = Vec::new();
        let mut low_dets = Vec::new();
        for det in deduplicated {
            if self.is_detection_masked_or_filtered(&det, cfg) {
                continue;
            }
            let high_thresh = cfg
                .objects
                .filters
                .get(&det.label)
                .map(|f| f.threshold)
                .unwrap_or(0.7);
            if det.score >= high_thresh {
                high_dets.push(det);
            } else {
                low_dets.push(det);
            }
        }

        let track_ids: Vec<String> = self.tracks.keys().cloned().collect();
        let mut matched_tracks = vec![false; track_ids.len()];
        let mut matched_high = vec![false; high_dets.len()];

        // Stage 1: Associate High-Confidence detections with existing tracks
        for (d_idx, det) in high_dets.iter().enumerate() {
            let mut best_t: Option<usize> = None;
            let mut best_score = 0.15f32;
            for (t_idx, tid) in track_ids.iter().enumerate() {
                if matched_tracks[t_idx] {
                    continue;
                }
                let trk = &self.tracks[tid];
                if trk.label != det.label {
                    continue;
                }
                let sim = association_similarity(&trk.bbox, &det.bbox);
                if sim > best_score {
                    best_score = sim;
                    best_t = Some(t_idx);
                }
            }
            if let Some(t_idx) = best_t {
                matched_tracks[t_idx] = true;
                matched_high[d_idx] = true;
                let tid = &track_ids[t_idx];
                self.apply_match(tid, det, frame_time);
            }
        }

        // Stage 2 (ByteTrack): Associate Low-Confidence detections with remaining unmatched tracks
        for det in &low_dets {
            let mut best_t: Option<usize> = None;
            let mut best_iou = 0.30f32;
            for (t_idx, tid) in track_ids.iter().enumerate() {
                if matched_tracks[t_idx] {
                    continue;
                }
                let trk = &self.tracks[tid];
                if trk.label != det.label || trk.disappeared_frames > 5 {
                    continue;
                }
                let iou = trk.bbox.iou(&det.bbox);
                if iou > best_iou {
                    best_iou = iou;
                    best_t = Some(t_idx);
                }
            }
            if let Some(t_idx) = best_t {
                matched_tracks[t_idx] = true;
                let tid = &track_ids[t_idx];
                self.apply_match(tid, det, frame_time);
            }
        }

        // 3. Handle unmatched tracks (preserving stationary tracks that were not in any scanned region)
        let mut ended_tracks = Vec::new();
        for (t_idx, tid) in track_ids.iter().enumerate() {
            if !matched_tracks[t_idx] {
                if let Some(trk) = self.tracks.get_mut(tid) {
                    let was_scanned = scanned_regions.iter().any(|r| r.intersects(&trk.bbox));
                    if trk.stationary && !was_scanned {
                        // Stationary track was intentionally skipped on this frame; keep it active
                        trk.disappeared_frames = 0;
                    } else {
                        trk.disappeared_frames += 1;
                    }
                }
                if self.tracks[tid].disappeared_frames > self.max_disappeared
                    && let Some(removed) = self.tracks.remove(tid)
                    && removed.frames_tracked >= self.min_initialized
                {
                    ended_tracks.push(removed);
                }
            }
        }

        // 4. Initialize new tracks from unmatched High-Confidence detections
        for (d_idx, det) in high_dets.into_iter().enumerate() {
            if matched_high[d_idx] {
                continue;
            }
            // Skip if it heavily overlaps an already-active track of the same label
            if self
                .tracks
                .values()
                .any(|t| t.label == det.label && t.bbox.iou(&det.bbox) > 0.60)
            {
                continue;
            }
            let suffix = format!(
                "{:06x}",
                (self.frame_seq as u32).wrapping_mul(0x9E37_79B9) ^ (d_idx as u32)
            );
            let id = format!("{frame_time:.6}-{suffix}");
            let zones = self.evaluate_zones(&det.label, &det.bbox);
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
                    has_snapshot: false,
                    vx: 0.0,
                    vy: 0.0,
                },
            );
        }

        let active: Vec<TrackedObject> = self.active_tracks().cloned().collect();
        (active, ended_tracks)
    }

    fn apply_match(&mut self, tid: &str, det: &DetectionResult, frame_time: f64) {
        let zones = self.evaluate_zones(&det.label, &det.bbox);
        let stationary_thresh = self.stationary_threshold;
        if let Some(trk) = self.tracks.get_mut(tid) {
            let (old_cx, old_cy) = trk.bbox.center();
            let (new_cx, new_cy) = det.bbox.center();
            let dx = new_cx - old_cx;
            let dy = new_cy - old_cy;
            trk.vx = 0.7 * trk.vx + 0.3 * dx;
            trk.vy = 0.7 * trk.vy + 0.3 * dy;

            let iou = trk.bbox.iou(&det.bbox);
            if iou > 0.85 && dx.hypot(dy) < 4.0 {
                trk.motionless_count += 1;
                if trk.motionless_count >= stationary_thresh {
                    trk.stationary = true;
                }
            } else {
                trk.motionless_count = 0;
                trk.stationary = false;
            }

            trk.bbox = det.bbox;
            trk.region = det.region;
            trk.score = det.score;
            if det.score > trk.top_score {
                trk.top_score = det.score;
            }
            trk.last_seen_time = frame_time;
            trk.frames_tracked += 1;
            trk.disappeared_frames = 0;
            trk.zones = zones.clone();
            for z in zones {
                if !trk.entered_zones.contains(&z) {
                    trk.entered_zones.push(z);
                }
            }
        }
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
    iou.max(dist_sim * 0.65)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytetracker_tracks_across_high_and_low_score_frames_and_preserves_stationary() {
        let mut cam_cfg = CameraConfig {
            enabled: true,
            ffmpeg: Default::default(),
            live: Default::default(),
            detect: Default::default(),
            record: Default::default(),
            snapshots: Default::default(),
            motion: Default::default(),
            objects: Default::default(),
            review: Default::default(),
            zones: Default::default(),
        };
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
}
