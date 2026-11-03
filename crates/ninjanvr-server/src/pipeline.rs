use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use ninjanvr_comms::SharedCommsHub;
use ninjanvr_config::{CameraConfig, NinjaConfig};
use ninjanvr_db::{EventDto, NinjaDatabase, ReviewItemDto};
use ninjanvr_infer::AiDetectorEngine;
use ninjanvr_media::{SharedActivityLedger, YuvFrame, backup_alert_clip, resolve_ffmpeg_bin};
use ninjanvr_track::{
    ByteTracker, TrackedAttribute, TrackedObject, TriggerProvenance, compute_attribute_quality,
    should_replace_snapshot,
};
pub use ninjanvr_vision::classify_dominant_color_tag;
use ninjanvr_vision::{
    BoundingBox, CameraRoiStateMonitor, RoiStateTransition, SimdMotionDetector,
    annotate_rgb_image_with_boxes, apply_privacy_masks_from_value, check_frame_tamper,
    compute_crop_dhash64, compute_inference_regions, crop_yuv420p_to_tensor_nhwc,
    encode_image_jpeg, encode_image_webp, is_monochrome_ir, yuv420p_to_rgb_image,
    yuv420p_to_rgb_image_scaled,
};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Detects whether a YUV420p frame is in monochrome IR night mode based on U/V chroma plane variance.
pub fn is_frame_night_ir(yuv_data: &[u8], width: u32, height: u32) -> bool {
    is_monochrome_ir(yuv_data, width, height)
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
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
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

#[derive(Clone, Debug, Default)]
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
}

pub struct VisionPipeline {
    pub config: Arc<RwLock<NinjaConfig>>,
    pub db: NinjaDatabase,
    pub comms: SharedCommsHub,
    pub detector: Arc<AiDetectorEngine>,
    pub activity_ledger: SharedActivityLedger,
    pub latest_frames: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    pub latest_yuv_frames: SharedLatestYuvFrames,
    pub camera_metrics: SharedCameraMetrics,
    pub media_dir: PathBuf,
    pub log_hub: crate::LogBufferHub,
    pub ptz_active_presets: Arc<RwLock<HashMap<String, Option<String>>>>,
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

    async fn run_camera_vision_loop(&self, camera: String, cam_cfg: CameraConfig, sub_url: String) {
        let width = cam_cfg.detect.width.max(160);
        let height = cam_cfg.detect.height.max(120);
        let fps = cam_cfg.detect.fps.clamp(1, 30);
        let frame_byte_len = ((width as usize) * (height as usize) * 3) / 2;

        let mut motion_detector = SimdMotionDetector::new(width, height, cam_cfg.motion.clone());
        let mut roi_monitor = CameraRoiStateMonitor::new(&cam_cfg.state_rois);
        let mut tracker = ByteTracker::new(&camera, width, height, &cam_cfg);
        let (model_w, model_h, _) = self.detector.model_dimensions();
        let mut tensor_buf = vec![0u8; (model_w * model_h * 3) as usize];
        let mut active_review: Option<ActiveReviewState> = None;
        let mut track_snapshots: HashMap<String, TrackPersistSnapshot> = HashMap::new();

        let ffmpeg_bin = resolve_ffmpeg_bin();
        let cam_arc: Arc<str> = Arc::from(camera.as_str());

        loop {
            let vf_arg = format!("fps={fps},scale={width}:{height}");
            let fps_str = fps.to_string();
            let mut cmd = Command::new(&ffmpeg_bin);
            cmd.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-probesize",
                "32768",
                "-analyzeduration",
                "500000",
                "-threads",
                "1",
                "-rtsp_transport",
                "tcp",
                "-timeout",
                "10000000",
                "-i",
                &sub_url,
                "-r",
                &fps_str,
                "-vf",
                &vf_arg,
                "-threads",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "yuv420p",
                "pipe:1",
            ])
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
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                continue;
            };

            let child_pid = child.id().unwrap_or(std::process::id());
            info!(
                "Started substream vision pipeline for {camera} ({}x{} @ {} FPS, pid={child_pid})",
                width, height, fps
            );

            self.camera_metrics.write().await.insert(
                camera.clone(),
                CameraRuntimeMetrics {
                    camera_fps: fps as f64,
                    process_fps: fps as f64,
                    detection_fps: 0.0,
                    pid: child_pid,
                    last_frame_ts: unix_now_f64(),
                    night_mode: false,
                },
            );

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

                while stdout
                    .read_exact(Arc::<Vec<u8>>::make_mut(&mut frame_data))
                    .await
                    .is_ok()
                {
                    let now = unix_now_f64();
                    frame_count += 1;
                    if frame_count == 1 && self.comms.set_camera_availability(&camera, true) {
                        self.log_hub.push_ninjanvr(
                            "INFO",
                            &format!("ninjanvr.camera.{camera}"),
                            "Camera stream reconnected (ONLINE)",
                        );
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
                    if let Some(active_preset_opt) =
                        self.ptz_active_presets.read().await.get(&camera).cloned()
                    {
                        tracker.set_active_preset(active_preset_opt.as_deref());
                    }

                    let _ = self.evaluate_camera_tamper(
                        &camera,
                        &yuv_frame.data,
                        width,
                        height,
                        now,
                        &mut consecutive_tamper_frames,
                        tamper_threshold_frames,
                    );

                    // 1. SIMD Motion Detection on Y-plane
                    let motion_boxes = if rt_flags.enabled && rt_flags.detect {
                        motion_detector.detect(&yuv_frame)
                    } else {
                        Vec::new()
                    };
                    let motion_px: u32 = motion_boxes.iter().map(|b| b.area()).sum();

                    if rt_flags.enabled && rt_flags.detect && !roi_monitor.is_empty() {
                        let _ = self.evaluate_camera_roi_states(
                            &camera,
                            &yuv_frame.data,
                            width,
                            height,
                            now,
                            &motion_boxes,
                            &mut roi_monitor,
                            false,
                        );
                    }

                    // 2. Dynamic Region Cropping & AI Object Detection (only when motion or non-stationary objects exist)
                    let active_det = self
                        .detector
                        .named_detector(cam_cfg.detect.detector.as_deref())
                        .unwrap_or_else(|| self.detector.clone());
                    let (model_w, model_h, bgr_order) = active_det.model_dimensions();
                    let needed_tensor_len = (model_w * model_h * 3) as usize;
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

                    let mut raw_detections = Vec::new();
                    for region in &regions {
                        crop_yuv420p_to_tensor_nhwc(
                            &yuv_frame,
                            region,
                            model_w,
                            model_h,
                            bgr_order,
                            &mut tensor_buf,
                        );
                        detect_calls += 1;
                        let mut dets =
                            active_det.detect_region(&tensor_buf, *region, width, height, 0.40);
                        dets.retain(|d| {
                            motion_boxes.iter().any(|mb| mb.intersects(&d.bbox))
                                || tracked_boxes.iter().any(|tb| tb.intersects(&d.bbox))
                        });
                        raw_detections.extend(dets);
                    }

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

                    if frame_count == 1 || frame_count.is_multiple_of(150) {
                        let annotated: Vec<(BoundingBox, [u8; 3])> = active_tracks
                            .iter()
                            .map(|t| (t.bbox, [31, 119, 180]))
                            .collect();
                        let mut rgb =
                            yuv420p_to_rgb_image_scaled(&yuv_frame, None, &annotated, Some(360));
                        apply_privacy_masks_from_value(&mut rgb, &cam_cfg.privacy_mask);
                        if let Ok(jpeg_bytes) = encode_image_jpeg(&rgb) {
                            let clips_dir = self.media_dir.join("clips");
                            if std::fs::create_dir_all(&clips_dir).is_ok() {
                                let disk_latest = clips_dir.join(format!("latest_{camera}.jpg"));
                                let _ = std::fs::write(&disk_latest, &jpeg_bytes);
                            }
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
                }
            }

            {
                let mut m_guard = self.camera_metrics.write().await;
                if let Some(m) = m_guard.get_mut(&camera) {
                    m.camera_fps = 0.0;
                    m.process_fps = 0.0;
                    m.detection_fps = 0.0;
                }
            }
            if self.comms.set_camera_availability(&camera, false) {
                self.log_hub.push_ninjanvr(
                    "WARNING",
                    &format!("ninjanvr.camera.{camera}"),
                    "Camera stream disconnected (OFFLINE)",
                );
            }
            let _ = child.wait().await;
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    /// Evaluates a YUV420p frame for camera tamper / occlusion hysteresis (`tamper_threshold_frames`
    /// consecutive blind/flat frames) and updates `CommsHub`, `LogBufferHub`, and SQLite `reviewsegment`.
    /// Returns `true` when the camera is currently in an active tamper state.
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate_camera_tamper(
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
                    let _ = self.db.insert_review_segment(&ReviewItemDto {
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
                    });
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
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate_camera_roi_states(
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
                self.emit_roi_anomaly_review_alert(camera, tr, now);
            }
        }
        transitions
    }

    /// Records a classified ROI state observation into `roi_monitor` for `roi_name`,
    /// publishing state transitions via `CommsHub` and persisting a `ReviewSegment` alert
    /// when `anomaly_state` persists `>= anomaly_timeout_sec`.
    pub fn record_roi_state_observation(
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
            self.emit_roi_anomaly_review_alert(camera, &tr, now);
        }
        Some(tr)
    }

    fn emit_roi_anomaly_review_alert(&self, camera: &str, tr: &RoiStateTransition, now: f64) {
        self.log_hub.push_ninjanvr(
            "WARNING",
            &format!("ninjanvr.vision.{camera}"),
            &format!(
                "ROI '{}' remained in anomalous state '{}' for {:.0}s",
                tr.roi_name, tr.current_state, tr.anomaly_elapsed_sec
            ),
        );
        let alert_id = format!("{now:.6}-roi-{}-{}", tr.roi_name, random_suffix());
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
        let _ = self.db.insert_review_segment(&dto);
        self.comms.publish_json(
            "reviews",
            &json!({
                "type": "new",
                "before": dto,
                "after": dto
            }),
        );
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
        let frame = ctx.frame;
        let now = ctx.now;
        let review_dir = self.media_dir.join("clips").join("review");
        let clips_dir = self.media_dir.join("clips");

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
            let prof_cfg = cfg_guard
                .profiles
                .get(&active_profile)
                .or_else(|| cfg_guard.profiles.get(&active_profile.to_ascii_uppercase()));
            let sup_rec = prof_cfg
                .map(|p| {
                    p.disabled_record_cameras
                        .iter()
                        .any(|c| c == camera || c == "*")
                })
                .unwrap_or(false);
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
        for trk in active_tracks {
            let prev = track_snapshots.get(&trk.id).cloned();
            let is_new = prev.is_none();
            let prev_zones: Vec<String> =
                prev.as_ref().map(|p| p.zones.clone()).unwrap_or_default();
            let newly_entered_zones: Vec<String> = trk
                .entered_zones
                .iter()
                .filter(|z| !prev_zones.contains(z))
                .cloned()
                .collect();
            let prev_best_score = prev.as_ref().map(|p| p.best_score).unwrap_or(0.0);
            let prev_best_attr_quality =
                prev.as_ref().map(|p| p.best_attribute_score).unwrap_or(0.0);
            let attr_q_from_boxes = trk
                .attributes
                .iter()
                .filter(|a| {
                    a.label.eq_ignore_ascii_case("face")
                        || a.label.eq_ignore_ascii_case("license_plate")
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
            let is_better_score = prev
                .as_ref()
                .map(|p| trk.score > p.best_score + 0.05)
                .unwrap_or(true);
            let is_better_attr = (prev_best_attr_quality <= 0.0 && new_attr_quality > 0.0)
                || new_attr_quality > prev_best_attr_quality + 0.02;
            let state_changed = prev
                .as_ref()
                .map(|p| p.zones != trk.entered_zones || p.stationary != trk.stationary)
                .unwrap_or(false);

            let lpr_consensus = if lpr_cfg.enabled {
                if let Some(ref cand) = trk.inferred_sub_label
                    && ninjanvr_infer::validate_license_plate(cand, lpr_cfg.format_regex.as_deref())
                {
                    let read_score = trk
                        .attributes
                        .iter()
                        .find(|a| a.label.eq_ignore_ascii_case("license_plate"))
                        .map(|a| a.score)
                        .unwrap_or(trk.score);
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
            let recognized_license_plate = lpr_consensus.as_ref().map(|c| c.plate.clone());
            let mut effective_sub_label = lpr_consensus
                .as_ref()
                .and_then(|c| c.sub_label.clone())
                .or_else(|| trk.inferred_sub_label.clone())
                .or_else(|| prev.as_ref().and_then(|p| p.sub_label.clone()));
            let mut effective_sub_label_score: Option<f32> =
                prev.as_ref().and_then(|p| p.sub_label_score);
            let mut merged_attrs: Vec<TrackedAttribute> = prev
                .as_ref()
                .map(|p| p.detected_attributes.clone())
                .unwrap_or_default();
            for attr in &trk.attributes {
                if let Some(existing) = merged_attrs.iter_mut().find(|a| a.label == attr.label) {
                    if attr.score >= existing.score || attr.area_px >= existing.area_px {
                        *existing = attr.clone();
                    }
                } else {
                    merged_attrs.push(attr.clone());
                }
            }
            if face_cfg.enabled
                && trk.label == "person"
                && (effective_sub_label.is_none()
                    || effective_sub_label.as_deref() == Some("unknown"))
            {
                let face_attr_bbox = merged_attrs
                    .iter()
                    .find(|a| a.label.eq_ignore_ascii_case("face"))
                    .map(|a| a.bbox);
                if let Some(face_crop_box) = ninjanvr_infer::resolve_face_crop_bbox(
                    trk.bbox,
                    face_attr_bbox,
                    frame.width,
                    frame.height,
                    face_cfg.min_face_area,
                ) {
                    let face_rgb =
                        yuv420p_to_rgb_image_scaled(frame, Some(face_crop_box), &[], Some(112));
                    if let Some(m) = self.detector.match_face_crop(
                        &faces_dir,
                        face_rgb.as_raw(),
                        face_cfg.similarity_threshold,
                        face_cfg.unknown_alert,
                    ) {
                        effective_sub_label = Some(m.sub_label);
                        effective_sub_label_score = Some(m.score);
                    }
                }
            }
            let attrs_changed = prev
                .as_ref()
                .map(|p| {
                    p.sub_label != effective_sub_label
                        || p.sub_label_score != effective_sub_label_score
                        || p.detected_attributes != merged_attrs
                })
                .unwrap_or(!merged_attrs.is_empty() || effective_sub_label.is_some());

            let norm_box = trk.bbox.to_normalized(frame.width, frame.height);
            let crop_dhash64 =
                compute_crop_dhash64(&frame.data, frame.width, frame.height, trk.bbox);
            let mut suppressed_fp = prev.as_ref().is_some_and(|p| p.suppressed_fp);
            let mut matched_fp_sig_id: Option<i64> = None;
            if is_new
                && !suppressed_fp
                && let Ok(Some(sig_val)) = self.db.match_false_positive_signature(
                    camera,
                    &trk.label,
                    norm_box,
                    Some(crop_dhash64),
                )
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
            let mut ai_verify_triggered = prev.as_ref().is_some_and(|p| p.ai_verify_triggered);

            if is_new || is_better_score || is_better_attr || state_changed || attrs_changed {
                let best_score = prev_best_score.max(trk.score);
                let best_attr_score = prev_best_attr_quality.max(new_attr_quality);
                let should_trigger_ai = !suppressed_fp
                    && !ai_verify_triggered
                    && genai_cfg.enabled
                    && genai_cfg.verify_ambiguous_alerts
                    && trk.score >= genai_cfg.ambiguous_score_min
                    && trk.score <= genai_cfg.ambiguous_score_max;
                if should_trigger_ai {
                    ai_verify_triggered = true;
                }
                track_snapshots.insert(
                    trk.id.clone(),
                    TrackPersistSnapshot {
                        best_score,
                        best_attribute_score: best_attr_score,
                        sub_label: effective_sub_label.clone(),
                        sub_label_score: effective_sub_label_score,
                        detected_attributes: merged_attrs.clone(),
                        zones: trk.entered_zones.clone(),
                        stationary: trk.stationary,
                        dhash64: Some(crop_dhash64),
                        suppressed_fp,
                        ai_verify_triggered,
                    },
                );

                let mut thumb_b64 = None;
                let replace_snap = should_replace_snapshot(
                    cam_cfg.snapshots.selection,
                    is_new,
                    prev_best_score,
                    trk.score,
                    prev_best_attr_quality,
                    new_attr_quality,
                    &trk.zones,
                    &trk.entered_zones,
                );
                if replace_snap {
                    let _ = std::fs::create_dir_all(&clips_dir);
                    let crop_img =
                        yuv420p_to_rgb_image_scaled(frame, Some(trk.region), &[], Some(175));
                    thumb_b64 = encode_image_webp(&crop_img)
                        .ok()
                        .map(|b| base64::engine::general_purpose::STANDARD.encode(b));

                    if snapshots_active {
                        let jpg_path = clips_dir.join(format!("{}.jpg", trk.id));
                        let webp_path = clips_dir.join(format!("{camera}-{}.webp", trk.id));
                        let clean_jpg_path = clips_dir.join(format!("{}-clean.jpg", trk.id));
                        let clean_webp_path =
                            clips_dir.join(format!("{camera}-{}-clean.webp", trk.id));

                        let mut clean_img = yuv420p_to_rgb_image(frame, None, &[]);
                        apply_privacy_masks_from_value(&mut clean_img, &cam_cfg.privacy_mask);
                        if let Ok(clean_webp) = encode_image_webp(&clean_img) {
                            let _ = std::fs::write(&clean_webp_path, clean_webp);
                        }
                        if let Ok(clean_jpg) = encode_image_jpeg(&clean_img) {
                            let _ = std::fs::write(&clean_jpg_path, clean_jpg);
                        }

                        let mut full_img = clean_img;
                        annotate_rgb_image_with_boxes(&mut full_img, &[(trk.bbox, [31, 119, 180])]);
                        if let Ok(jpg) = encode_image_jpeg(&full_img) {
                            let _ = std::fs::write(&jpg_path, jpg);
                        }
                        let mut thumb_img = yuv420p_to_rgb_image_scaled(
                            frame,
                            None,
                            &[(trk.bbox, [31, 119, 180])],
                            Some(360),
                        );
                        apply_privacy_masks_from_value(&mut thumb_img, &cam_cfg.privacy_mask);
                        if let Ok(webp) = encode_image_webp(&thumb_img) {
                            let _ = std::fs::write(&webp_path, webp);
                        }

                        // Async high-resolution main-stream snapshot upgrade via normalized bbox projection
                        if cam_cfg.snapshots.high_res && cam_cfg.snapshots.from_main_stream {
                            let cam_owned = camera.to_string();
                            let det_bbox = trk.bbox;
                            let det_w = frame.width;
                            let det_h = frame.height;
                            let draw_bbox = cam_cfg.snapshots.bounding_box;
                            let priv_mask = cam_cfg.privacy_mask.clone();
                            tokio::spawn(async move {
                                let url =
                                    format!("http://127.0.0.1:1984/api/frame.jpeg?src={cam_owned}");
                                let Ok(client) = reqwest::Client::builder()
                                    .timeout(std::time::Duration::from_millis(1500))
                                    .build()
                                else {
                                    return;
                                };
                                if let Ok(resp) = client.get(&url).send().await
                                    && resp.status().is_success()
                                    && let Ok(bytes) = resp.bytes().await
                                    && let Ok(dyn_img) = image::load_from_memory(&bytes)
                                {
                                    let mut hr_rgb = dyn_img.to_rgb8();
                                    if hr_rgb.width() >= det_w && hr_rgb.height() >= det_h {
                                        apply_privacy_masks_from_value(&mut hr_rgb, &priv_mask);
                                        if let Ok(hr_clean_webp) = encode_image_webp(&hr_rgb) {
                                            let _ = std::fs::write(&clean_webp_path, hr_clean_webp);
                                        }
                                        if let Ok(hr_clean_jpg) = encode_image_jpeg(&hr_rgb) {
                                            let _ = std::fs::write(&clean_jpg_path, hr_clean_jpg);
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
                                        }
                                        if let Ok(hr_jpg) = encode_image_jpeg(&hr_rgb) {
                                            let _ = std::fs::write(&jpg_path, hr_jpg);
                                        }
                                        if let Ok(hr_webp) = encode_image_webp(&hr_rgb) {
                                            let _ = std::fs::write(&webp_path, hr_webp);
                                        }
                                    }
                                }
                            });
                        }
                    }
                }

                let norm_reg = trk.region.to_normalized(frame.width, frame.height);
                let mut trk_for_sev = trk.clone();
                if trk_for_sev.inferred_sub_label.is_none() {
                    trk_for_sev.inferred_sub_label = effective_sub_label.clone();
                }
                let severity = if suppressed_fp || !cam_rt_flags.alerts {
                    "detection"
                } else {
                    compute_track_severity_for_profile(
                        cam_cfg,
                        &trk_for_sev,
                        &active_profile,
                        camera_suppressed_in_profile,
                    )
                };
                let area = trk.bbox.area();
                let ratio = (trk.bbox.width() as f32) / (trk.bbox.height().max(1) as f32);

                let color_attr =
                    classify_dominant_color_tag(&frame.data, frame.width, frame.height, trk.bbox);
                let mut attr_tags = vec![color_attr.to_string()];
                for a in &merged_attrs {
                    if !attr_tags.contains(&a.label) {
                        attr_tags.push(a.label.clone());
                    }
                }
                if let Some(ref plate_str) = recognized_license_plate
                    && !attr_tags.contains(plate_str)
                {
                    attr_tags.push(plate_str.clone());
                }
                let face_box = merged_attrs
                    .iter()
                    .find(|a| a.label.eq_ignore_ascii_case("face"))
                    .map(|a| a.norm_bbox);
                let license_plate_box = merged_attrs
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

                let ev_dto = EventDto {
                    id: trk.id.clone(),
                    camera: camera.to_string(),
                    label: trk.label.clone(),
                    zones: json!(trk.entered_zones),
                    start_time: trk.start_time,
                    end_time: None,
                    has_clip: record_active,
                    has_snapshot: snapshots_active,
                    plus_id: None,
                    retain_indefinitely: false,
                    sub_label: effective_sub_label.clone(),
                    top_score: Some(best_score as f64),
                    false_positive: Some(suppressed_fp),
                    box_coords: Some(json!(norm_box)),
                    data: json!({
                        "box": norm_box,
                        "region": norm_reg,
                        "dhash64": crop_dhash64,
                        "suppressed_by_fp_signature": matched_fp_sig_id,
                        "score": trk.score,
                        "top_score": best_score,
                        "sub_label": effective_sub_label,
                        "sub_label_score": effective_sub_label_score,
                        "attributes": attr_tags,
                        "detected_attributes": merged_attrs,
                        "face_box": face_box,
                        "license_plate_box": license_plate_box,
                        "recognized_license_plate": recognized_license_plate,
                        "type": "object",
                        "max_severity": severity,
                        "provenance": trk.provenance
                    }),
                    thumbnail: thumb_b64,
                    area: Some(area),
                    ratio: Some(ratio),
                    score: Some(trk.score),
                    stationary: trk.stationary,
                };
                let _ = self.db.upsert_event(&ev_dto);
                if let Some(ref sl) = effective_sub_label {
                    let _ = self.db.set_event_sub_label_with_score(
                        &trk.id,
                        Some(sl),
                        effective_sub_label_score.map(|v| v as f64),
                    );
                }
                if should_trigger_ai {
                    let cfg_clone = self.config.clone();
                    let db_clone = self.db.clone();
                    let media_clone = self.media_dir.clone();
                    let ev_id_owned = trk.id.clone();
                    tokio::spawn(async move {
                        let _ = crate::verify_event_with_genai_components(
                            cfg_clone,
                            db_clone,
                            media_clone,
                            &ev_id_owned,
                        )
                        .await;
                    });
                }
                if is_new {
                    let _ = self.db.insert_timeline_entry(
                        now,
                        camera,
                        "tracked_object",
                        Some(&trk.id),
                        "visible",
                        &json!({
                            "box": norm_box,
                            "label": trk.label,
                            "region": norm_reg,
                            "score": trk.score,
                            "top_score": best_score
                        }),
                    );
                }
                if !newly_entered_zones.is_empty() {
                    let _ = self.db.insert_timeline_entry(
                        now,
                        camera,
                        "tracked_object",
                        Some(&trk.id),
                        "entered_zone",
                        &json!({
                            "box": norm_box,
                            "label": trk.label,
                            "region": norm_reg,
                            "zones": trk.entered_zones,
                            "entered_zones": newly_entered_zones,
                            "score": trk.score,
                            "top_score": best_score
                        }),
                    );
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
                            trk.id, trk.label, trk.score, area
                        ),
                    );
                }
            }

            if suppressed_fp {
                continue;
            }

            // Update or open ReviewSegment if enabled
            let mut trk_for_rev_sev = trk.clone();
            if trk_for_rev_sev.inferred_sub_label.is_none() {
                trk_for_rev_sev.inferred_sub_label = effective_sub_label.clone();
            }
            let track_sev = if !cam_rt_flags.alerts {
                "detection"
            } else {
                compute_track_severity_for_profile(
                    cam_cfg,
                    &trk_for_rev_sev,
                    &active_profile,
                    camera_suppressed_in_profile,
                )
            };
            let is_alert = track_sev == "alert";
            let detection_zone_ok = cam_cfg.review.detections.required_zones.is_empty()
                || trk
                    .entered_zones
                    .iter()
                    .any(|z| cam_cfg.review.detections.required_zones.contains(z));
            if !is_alert && (!cam_cfg.review.detections.enabled || !detection_zone_ok) {
                continue;
            }

            let rev = active_review.get_or_insert_with(|| {
                let rev_id = format!("{now:.6}-{}", random_suffix());
                let thumb_rel =
                    format!("/media/ninjanvr/clips/review/thumb-{camera}-{rev_id}.webp");
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
                    sub_label_score: effective_sub_label_score,
                    detected_attributes: Vec::new(),
                    zones: Vec::new(),
                    provenance: Some(trk.provenance.clone()),
                    is_new: true,
                    dirty: true,
                    prev_dto: None,
                }
            });

            rev.last_activity_time = now;
            let promoted_to_alert = is_alert && rev.severity != "alert";
            if promoted_to_alert {
                rev.severity = "alert".to_string();
                rev.dirty = true;
            }
            if !rev.detections.contains(&trk.id) {
                rev.detections.push(trk.id.clone());
                rev.dirty = true;
            }
            if !rev.objects.contains(&trk.label) {
                rev.objects.push(trk.label.clone());
                rev.dirty = true;
            }
            if let Some(ref sl) = effective_sub_label
                && !rev.sub_labels.contains(sl)
            {
                rev.sub_labels.push(sl.clone());
                rev.dirty = true;
            }
            if let Some(sc) = effective_sub_label_score
                && rev
                    .sub_label_score
                    .map(|prev_sc| sc > prev_sc)
                    .unwrap_or(true)
            {
                rev.sub_label_score = Some(sc);
                rev.dirty = true;
            }
            for attr in &merged_attrs {
                if !rev
                    .detected_attributes
                    .iter()
                    .any(|a| a.label == attr.label)
                {
                    rev.detected_attributes.push(attr.clone());
                    rev.dirty = true;
                }
            }
            for z in &trk.entered_zones {
                if !rev.zones.contains(z) {
                    rev.zones.push(z.clone());
                    rev.dirty = true;
                }
            }
            if trk.score > rev.best_score + 0.05 || promoted_to_alert {
                rev.best_score = trk.score;
                rev.thumb_time = now;
                rev.provenance = Some(trk.provenance.clone());
                let _ = std::fs::create_dir_all(&review_dir);
                let mut rev_img = yuv420p_to_rgb_image_scaled(
                    frame,
                    None,
                    &[(trk.bbox, [31, 119, 180])],
                    Some(180),
                );
                apply_privacy_masks_from_value(&mut rev_img, &cam_cfg.privacy_mask);
                if let Ok(webp) = encode_image_webp(&rev_img) {
                    let fname = format!("thumb-{camera}-{}.webp", rev.id);
                    let _ = std::fs::write(review_dir.join(fname), webp);
                }
            }
        }

        // Finalize ended tracks
        for trk in ended_tracks {
            let removed_snap = track_snapshots.remove(&trk.id);
            let suppressed_fp = removed_snap.as_ref().is_some_and(|s| s.suppressed_fp);
            let crop_dhash64 = removed_snap
                .as_ref()
                .and_then(|s| s.dhash64)
                .unwrap_or_else(|| {
                    compute_crop_dhash64(&frame.data, frame.width, frame.height, trk.bbox)
                });
            let best_score = removed_snap
                .as_ref()
                .map(|s| s.best_score.max(trk.top_score))
                .unwrap_or(trk.top_score);
            let lpr_consensus = if lpr_cfg.enabled {
                self.detector.finalize_lpr_track(&trk.id, &lpr_cfg)
            } else {
                None
            };
            let recognized_license_plate = lpr_consensus.as_ref().map(|c| c.plate.clone());
            let effective_sub_label = lpr_consensus
                .as_ref()
                .and_then(|c| c.sub_label.clone())
                .or_else(|| trk.inferred_sub_label.clone())
                .or_else(|| removed_snap.as_ref().and_then(|s| s.sub_label.clone()));
            let effective_sub_label_score = removed_snap.as_ref().and_then(|s| s.sub_label_score);
            let mut merged_attrs = removed_snap
                .map(|s| s.detected_attributes)
                .unwrap_or_default();
            for attr in &trk.attributes {
                if let Some(existing) = merged_attrs.iter_mut().find(|a| a.label == attr.label) {
                    if attr.score >= existing.score || attr.area_px >= existing.area_px {
                        *existing = attr.clone();
                    }
                } else {
                    merged_attrs.push(attr.clone());
                }
            }
            let norm_box = trk.bbox.to_normalized(frame.width, frame.height);
            let norm_reg = trk.region.to_normalized(frame.width, frame.height);
            let mut trk_for_sev = trk.clone();
            if trk_for_sev.inferred_sub_label.is_none() {
                trk_for_sev.inferred_sub_label = effective_sub_label.clone();
            }
            let severity = if suppressed_fp || !cam_rt_flags.alerts {
                "detection"
            } else {
                compute_track_severity_for_profile(
                    cam_cfg,
                    &trk_for_sev,
                    &active_profile,
                    camera_suppressed_in_profile,
                )
            };
            let area = trk.bbox.area();
            let ratio = (trk.bbox.width() as f32) / (trk.bbox.height().max(1) as f32);
            let color_attr =
                classify_dominant_color_tag(&frame.data, frame.width, frame.height, trk.bbox);
            let mut attr_tags = vec![color_attr.to_string()];
            for a in &merged_attrs {
                if !attr_tags.contains(&a.label) {
                    attr_tags.push(a.label.clone());
                }
            }
            if let Some(ref plate_str) = recognized_license_plate
                && !attr_tags.contains(plate_str)
            {
                attr_tags.push(plate_str.clone());
            }
            let face_box = merged_attrs
                .iter()
                .find(|a| a.label.eq_ignore_ascii_case("face"))
                .map(|a| a.norm_bbox);
            let license_plate_box = merged_attrs
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

            let ev_dto = EventDto {
                id: trk.id.clone(),
                camera: camera.to_string(),
                label: trk.label.clone(),
                zones: json!(trk.entered_zones),
                start_time: trk.start_time,
                end_time: Some(trk.last_seen_time),
                has_clip: record_active,
                has_snapshot: snapshots_active,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: effective_sub_label.clone(),
                top_score: Some(best_score as f64),
                false_positive: Some(suppressed_fp),
                box_coords: Some(json!(norm_box)),
                data: json!({
                    "box": norm_box,
                    "region": norm_reg,
                    "dhash64": crop_dhash64,
                    "score": trk.score,
                    "top_score": best_score,
                    "sub_label": effective_sub_label,
                    "sub_label_score": effective_sub_label_score,
                    "attributes": attr_tags,
                    "detected_attributes": merged_attrs,
                    "face_box": face_box,
                    "license_plate_box": license_plate_box,
                    "recognized_license_plate": recognized_license_plate,
                    "type": "object",
                    "max_severity": severity,
                    "provenance": trk.provenance
                }),
                thumbnail: None,
                area: Some(area),
                ratio: Some(ratio),
                score: Some(trk.score),
                stationary: trk.stationary,
            };
            let _ = self.db.upsert_event(&ev_dto);
            if let Some(ref sl) = effective_sub_label {
                let _ = self.db.set_event_sub_label_with_score(
                    &trk.id,
                    Some(sl),
                    effective_sub_label_score.map(|v| v as f64),
                );
            }
            let _ = self.db.insert_timeline_entry(
                now,
                camera,
                "tracked_object",
                Some(&trk.id),
                "gone",
                &json!({
                    "box": norm_box,
                    "label": trk.label,
                    "region": norm_reg,
                    "score": trk.score,
                    "top_score": best_score
                }),
            );
            self.comms.publish_json(
                "events",
                &json!({
                    "type": "end",
                    "before": ev_dto,
                    "after": ev_dto
                }),
            );
        }

        // Flush or close active review segment with {type, before, after} schema
        if let Some(rev) = active_review {
            let cutoff_secs = if rev.severity == "alert" {
                cam_cfg.review.alerts.cutoff_time.max(15) as f64
            } else {
                cam_cfg.review.detections.cutoff_time.max(15) as f64
            };
            let is_expired =
                active_tracks.is_empty() && (now - rev.last_activity_time) >= cutoff_secs;

            if rev.dirty || is_expired {
                let end_time = if is_expired {
                    Some(rev.last_activity_time)
                } else {
                    None
                };
                let dto = rev.to_dto(camera, end_time);
                let _ = self.db.insert_review_segment(&dto);

                let before_dto = rev.prev_dto.clone().unwrap_or_else(|| dto.clone());
                let msg_type = if is_expired {
                    "end"
                } else if rev.is_new {
                    "new"
                } else {
                    "update"
                };

                self.comms.publish_json(
                    "reviews",
                    &json!({
                        "type": msg_type,
                        "before": before_dto,
                        "after": dto
                    }),
                );

                rev.is_new = false;
                rev.dirty = false;
                rev.prev_dto = Some(dto);

                let reached_60s_checkpoint = (now - rev.last_backup_time) >= 60.0;
                if is_expired || reached_60s_checkpoint {
                    rev.last_backup_time = now;
                    let bak_cfg = &cam_cfg.record.alert_backup;
                    if bak_cfg.enabled
                        && bak_cfg.severities.iter().any(|s| s == &rev.severity)
                        && let Some(ref target_dir) = bak_cfg.target_dir
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
                if is_expired {
                    *active_review = None;
                }
            }
        }
    }
}

pub fn is_camera_alert_suppressed_in_profile(
    cfg: &NinjaConfig,
    active_profile: &str,
    camera: &str,
) -> bool {
    let prof_cfg = cfg
        .profiles
        .get(active_profile)
        .or_else(|| cfg.profiles.get(&active_profile.to_ascii_uppercase()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use ninjanvr_comms::CommsHub;
    use ninjanvr_config::{ProfileOverrideConfig, SnapshotSelectionPolicy};
    use ninjanvr_db::ReviewFilter;
    use ninjanvr_media::new_activity_ledger;
    use ninjanvr_track::TriggerProvenance;
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
            camera_metrics: Arc::new(RwLock::new(HashMap::new())),
            media_dir,
            log_hub: crate::LogBufferHub::new(),
            ptz_active_presets: Arc::new(RwLock::new(HashMap::new())),
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

    #[test]
    fn evaluate_camera_tamper_triggers_after_sustained_luma_drop_and_clears_on_recovery() {
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
        assert!(!pipeline.evaluate_camera_tamper(
            "front_door",
            &normal_scene,
            w,
            h,
            1000.0,
            &mut consecutive,
            threshold
        ));
        assert_eq!(consecutive, 0);

        // Frames 1..3 of sustained luma drop increment counter without triggering yet
        for i in 1..threshold {
            assert!(!pipeline.evaluate_camera_tamper(
                "front_door",
                &occluded_scene,
                w,
                h,
                1000.0 + i as f64,
                &mut consecutive,
                threshold
            ));
            assert_eq!(consecutive, i);
        }

        // Frame 4 reaches threshold: triggers tamper alert, broadcasts ON, and persists reviewsegment
        assert!(pipeline.evaluate_camera_tamper(
            "front_door",
            &occluded_scene,
            w,
            h,
            1004.0,
            &mut consecutive,
            threshold
        ));
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
        assert!(!pipeline.evaluate_camera_tamper(
            "front_door",
            &normal_scene,
            w,
            h,
            1005.0,
            &mut consecutive,
            threshold
        ));
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
                    .map(|p| p["type"] == "new" && p["after"]["id"] == trk.id)
                    .unwrap_or(false)
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
                    .map(|p| p["type"] == "update")
                    .unwrap_or(false)
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
                    .map(|p| p["type"] == "end" && p["after"]["id"] == trk.id)
                    .unwrap_or(false)
        }));
        assert!(ws_msgs_end.iter().any(|m| {
            m["topic"] == "reviews"
                && serde_json::from_str::<Value>(m["payload"].as_str().unwrap_or("{}"))
                    .map(|p| p["type"] == "end" && p["after"]["id"] == rev_id)
                    .unwrap_or(false)
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

    #[test]
    fn evaluate_camera_roi_states_enforces_hysteresis_and_emits_anomaly_review_segment_after_timeout()
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
        let _ = pipeline.record_roi_state_observation(
            camera,
            &mut monitor,
            "gate",
            "open",
            0.90,
            1005.0,
        );
        let _ = pipeline.record_roi_state_observation(
            camera,
            &mut monitor,
            "gate",
            "open",
            0.90,
            1010.0,
        );
        let _ = pipeline.record_roi_state_observation(
            camera,
            &mut monitor,
            "gate",
            "closed",
            0.95,
            1015.0,
        );
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
        let _ = pipeline.record_roi_state_observation(
            camera,
            &mut monitor,
            "gate",
            "open",
            0.91,
            1020.0,
        );
        let _ = pipeline.record_roi_state_observation(
            camera,
            &mut monitor,
            "gate",
            "open",
            0.92,
            1025.0,
        );
        let tr_open = pipeline
            .record_roi_state_observation(camera, &mut monitor, "gate", "open", 0.93, 1030.0)
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
        let _ = pipeline.record_roi_state_observation(
            camera,
            &mut monitor,
            "gate",
            "open",
            0.93,
            1095.0,
        );
        let _ = pipeline.record_roi_state_observation(
            camera,
            &mut monitor,
            "gate",
            "closed",
            0.94,
            1100.0,
        );
        let _ = pipeline.record_roi_state_observation(
            camera,
            &mut monitor,
            "gate",
            "closed",
            0.95,
            1105.0,
        );
        let tr_rec = pipeline
            .record_roi_state_observation(camera, &mut monitor, "gate", "closed", 0.96, 1110.0)
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

        let _ = pipeline.evaluate_camera_roi_states(
            camera,
            &open_yuv,
            w,
            h,
            1190.0,
            &[],
            &mut monitor,
            true,
        );
        let _ = pipeline.evaluate_camera_roi_states(
            camera,
            &open_yuv,
            w,
            h,
            1195.0,
            &[],
            &mut monitor,
            true,
        );
        let trs_open2 = pipeline.evaluate_camera_roi_states(
            camera,
            &open_yuv,
            w,
            h,
            1200.0,
            &[],
            &mut monitor,
            true,
        );
        assert_eq!(trs_open2[0].current_state, "open");
        assert!(trs_open2[0].state_changed);

        // At t=1319.0 (119s < 120s): still no alert
        let _ = pipeline.evaluate_camera_roi_states(
            camera,
            &open_yuv,
            w,
            h,
            1319.0,
            &[],
            &mut monitor,
            true,
        );
        assert!(
            pipeline
                .db
                .query_reviews(&ReviewFilter::default())
                .unwrap()
                .is_empty()
        );

        // At t=1320.0 (120s >= 120s): triggers ReviewSegment alert
        let trs_alert = pipeline.evaluate_camera_roi_states(
            camera,
            &open_yuv,
            w,
            h,
            1320.0,
            &[],
            &mut monitor,
            true,
        );
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
}
