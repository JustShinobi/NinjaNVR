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
use ninjanvr_media::{SharedActivityLedger, YuvFrame, resolve_ffmpeg_bin};
use ninjanvr_track::{ByteTracker, TrackedObject, TriggerProvenance, should_replace_snapshot};
use ninjanvr_vision::{
    BoundingBox, SimdMotionDetector, annotate_rgb_image_with_boxes, compute_inference_regions,
    crop_yuv420p_to_tensor_nhwc, encode_image_jpeg, encode_image_webp, is_monochrome_ir,
    yuv420p_to_rgb_image, yuv420p_to_rgb_image_scaled,
};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

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

struct FrameLifecycleCtx<'a> {
    camera: &'a str,
    cam_cfg: &'a CameraConfig,
    frame: &'a YuvFrame,
    now: f64,
}

#[derive(Clone)]
struct TrackPersistSnapshot {
    best_score: f32,
    zones: Vec<String>,
    stationary: bool,
}

struct ActiveReviewState {
    id: String,
    start_time: f64,
    last_activity_time: f64,
    severity: String,
    thumb_path: String,
    thumb_time: f64,
    best_score: f32,
    detections: Vec<String>,
    objects: Vec<String>,
    zones: Vec<String>,
    provenance: Option<TriggerProvenance>,
    is_new: bool,
    dirty: bool,
    prev_dto: Option<ReviewItemDto>,
}

impl ActiveReviewState {
    fn to_dto(&self, camera: &str, end_time: Option<f64>) -> ReviewItemDto {
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
                "sub_labels": [],
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
    pub camera_metrics: SharedCameraMetrics,
    pub media_dir: PathBuf,
    pub log_hub: crate::LogBufferHub,
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
                    self.log_hub.push_frigate(
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
                    detection_fps: 1.2,
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

                while stdout
                    .read_exact(Arc::<Vec<u8>>::make_mut(&mut frame_data))
                    .await
                    .is_ok()
                {
                    let now = unix_now_f64();
                    frame_count += 1;
                    if frame_count == 1 && self.comms.set_camera_availability(&camera, true) {
                        self.log_hub.push_frigate(
                            "INFO",
                            &format!("ninjanvr.camera.{camera}"),
                            "Camera stream reconnected (ONLINE)",
                        );
                    }

                    let yuv_frame = YuvFrame {
                        camera: cam_arc.clone(),
                        timestamp: now,
                        width,
                        height,
                        data: frame_data.clone(),
                    };
                    let is_night_ir = is_monochrome_ir(&yuv_frame.data, width, height);
                    tracker.set_night_mode(is_night_ir);

                    // 1. SIMD Motion Detection on Y-plane
                    let motion_boxes = motion_detector.detect(&yuv_frame);
                    let motion_px: u32 = motion_boxes.iter().map(|b| b.area()).sum();

                    // 2. Dynamic Region Cropping & AI Object Detection (only when motion or non-stationary objects exist)
                    let (model_w, model_h, bgr_order) = self.detector.model_dimensions();
                    let needed_tensor_len = (model_w * model_h * 3) as usize;
                    if tensor_buf.len() != needed_tensor_len {
                        tensor_buf.resize(needed_tensor_len, 0);
                    }
                    let tracked_boxes = tracker.non_stationary_boxes(&motion_boxes);
                    let regions = compute_inference_regions(
                        width,
                        height,
                        model_w.max(model_h),
                        &motion_boxes,
                        &tracked_boxes,
                    );

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
                        let dets =
                            self.detector
                                .detect_region(&tensor_buf, *region, width, height, 0.40);
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

                    if activity_changed || !active_tracks.is_empty() {
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
                                    "sub_label": null
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

                    // 6. Update latest frame JPEG cache every 25 frames (~5s) at 360p to keep idle CPU near zero
                    if frame_count == 1 || frame_count.is_multiple_of(25) {
                        let annotated: Vec<(BoundingBox, [u8; 3])> = active_tracks
                            .iter()
                            .map(|t| (t.bbox, [31, 119, 180]))
                            .collect();
                        let rgb =
                            yuv420p_to_rgb_image_scaled(&yuv_frame, None, &annotated, Some(360));
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
                        self.camera_metrics
                            .write()
                            .await
                            .entry(camera.clone())
                            .or_default()
                            .last_frame_ts = now;
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
                self.log_hub.push_frigate(
                    "WARNING",
                    &format!("ninjanvr.camera.{camera}"),
                    "Camera stream disconnected (OFFLINE)",
                );
            }
            let _ = child.wait().await;
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    async fn process_lifecycle_events(
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

        for trk in active_tracks {
            let prev = track_snapshots.get(&trk.id);
            let is_new = prev.is_none();
            let is_better_score = prev
                .map(|p| trk.score > p.best_score + 0.05)
                .unwrap_or(true);
            let state_changed = prev
                .map(|p| p.zones != trk.entered_zones || p.stationary != trk.stationary)
                .unwrap_or(false);

            if is_new || is_better_score || state_changed {
                let prev_best_score = prev.map(|p| p.best_score).unwrap_or(0.0);
                let best_score = prev_best_score.max(trk.score);
                track_snapshots.insert(
                    trk.id.clone(),
                    TrackPersistSnapshot {
                        best_score,
                        zones: trk.entered_zones.clone(),
                        stationary: trk.stationary,
                    },
                );

                let mut thumb_b64 = None;
                let replace_snap = should_replace_snapshot(
                    cam_cfg.snapshots.selection,
                    is_new,
                    prev_best_score,
                    trk.score,
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

                    if cam_cfg.snapshots.enabled {
                        let jpg_path = clips_dir.join(format!("{}.jpg", trk.id));
                        let webp_path = clips_dir.join(format!("{camera}-{}.webp", trk.id));
                        let full_img =
                            yuv420p_to_rgb_image(frame, None, &[(trk.bbox, [31, 119, 180])]);
                        if let Ok(jpg) = encode_image_jpeg(&full_img) {
                            let _ = std::fs::write(&jpg_path, jpg);
                        }
                        let thumb_img = yuv420p_to_rgb_image_scaled(
                            frame,
                            None,
                            &[(trk.bbox, [31, 119, 180])],
                            Some(360),
                        );
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

                let norm_box = trk.bbox.to_normalized(frame.width, frame.height);
                let norm_reg = trk.region.to_normalized(frame.width, frame.height);
                let severity = compute_track_severity(cam_cfg, trk);
                let area = trk.bbox.area();
                let ratio = (trk.bbox.width() as f32) / (trk.bbox.height().max(1) as f32);

                let ev_dto = EventDto {
                    id: trk.id.clone(),
                    camera: camera.to_string(),
                    label: trk.label.clone(),
                    zones: json!(trk.entered_zones),
                    start_time: trk.start_time,
                    end_time: None,
                    has_clip: cam_cfg.record.enabled,
                    has_snapshot: cam_cfg.snapshots.enabled,
                    plus_id: None,
                    retain_indefinitely: false,
                    sub_label: None,
                    top_score: Some(best_score as f64),
                    false_positive: Some(false),
                    box_coords: Some(json!(norm_box)),
                    data: json!({
                        "box": norm_box,
                        "region": norm_reg,
                        "score": trk.score,
                        "top_score": best_score,
                        "attributes": [],
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
                    self.log_hub.push_frigate(
                        "INFO",
                        &format!("ninjanvr.detect.{camera}"),
                        &format!(
                            "Tracked object {}: {} (score={:.2}, area={})",
                            trk.id, trk.label, trk.score, area
                        ),
                    );
                }
            }

            // Update or open ReviewSegment if enabled
            let track_sev = compute_track_severity(cam_cfg, trk);
            let is_alert = track_sev == "alert";
            if !is_alert && !cam_cfg.review.detections.enabled {
                continue;
            }

            let rev = active_review.get_or_insert_with(|| {
                let rev_id = format!("{now:.6}-{}", random_suffix());
                let thumb_rel = format!("/media/frigate/clips/review/thumb-{camera}-{rev_id}.webp");
                ActiveReviewState {
                    id: rev_id,
                    start_time: now,
                    last_activity_time: now,
                    severity: track_sev.to_string(),
                    thumb_path: thumb_rel,
                    thumb_time: now,
                    best_score: 0.0,
                    detections: Vec::new(),
                    objects: Vec::new(),
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
                let rev_img = yuv420p_to_rgb_image_scaled(
                    frame,
                    None,
                    &[(trk.bbox, [31, 119, 180])],
                    Some(180),
                );
                if let Ok(webp) = encode_image_webp(&rev_img) {
                    let fname = format!("thumb-{camera}-{}.webp", rev.id);
                    let _ = std::fs::write(review_dir.join(fname), webp);
                }
            }
        }

        // Finalize ended tracks
        for trk in ended_tracks {
            let best_score = track_snapshots
                .remove(&trk.id)
                .map(|s| s.best_score.max(trk.top_score))
                .unwrap_or(trk.top_score);
            let norm_box = trk.bbox.to_normalized(frame.width, frame.height);
            let norm_reg = trk.region.to_normalized(frame.width, frame.height);
            let severity = compute_track_severity(cam_cfg, trk);
            let area = trk.bbox.area();
            let ratio = (trk.bbox.width() as f32) / (trk.bbox.height().max(1) as f32);
            let ev_dto = EventDto {
                id: trk.id.clone(),
                camera: camera.to_string(),
                label: trk.label.clone(),
                zones: json!(trk.entered_zones),
                start_time: trk.start_time,
                end_time: Some(trk.last_seen_time),
                has_clip: cam_cfg.record.enabled,
                has_snapshot: cam_cfg.snapshots.enabled,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(best_score as f64),
                false_positive: Some(false),
                box_coords: Some(json!(norm_box)),
                data: json!({
                    "box": norm_box,
                    "region": norm_reg,
                    "score": trk.score,
                    "top_score": best_score,
                    "attributes": [],
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

        // Flush or close active review segment with Frigate-compatible {type, before, after} schema
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

                if is_expired {
                    *active_review = None;
                }
            }
        }
    }
}

fn compute_track_severity(cam_cfg: &CameraConfig, trk: &TrackedObject) -> &'static str {
    if cam_cfg.review.alerts.enabled
        && cam_cfg.review.alerts.labels.contains(&trk.label)
        && (cam_cfg.review.alerts.required_zones.is_empty()
            || trk
                .entered_zones
                .iter()
                .any(|z| cam_cfg.review.alerts.required_zones.contains(z)))
    {
        "alert"
    } else {
        "detection"
    }
}
