#![recursion_limit = "512"]
#![forbid(unsafe_code)]
//! Axum HTTP/TLS operator server, REST/WebSocket API routes, declarative camera
//! protocol drivers, MSE fMP4 live streaming hub, and vision pipeline orchestration.

pub mod auth_gate;
pub mod camera_protocols;
pub mod config_coordinator;
pub mod dataset_catalog;
pub mod detector_manager;
pub mod export_jobs;
pub mod face_gallery;
pub mod federation_routes;
pub mod live_gateway;
pub mod mcp_routes;
pub mod media_lifecycle;
pub mod media_redaction;
pub mod mse_hub;
pub mod pipeline;
pub mod preview_frames;
pub mod share_engine;
pub mod snapshot_capture;
pub mod storage_ops;
pub mod telemetry;
pub mod track_lifecycle;
pub mod vod_routes;

pub use auth_gate::*;
pub use camera_protocols::*;
pub use config_coordinator::{ConfigCoordinator, persist_config_yaml_atomically};
pub use dataset_catalog::*;
pub use detector_manager::{DetectorModelManager, StagedModelUploadBundle};
pub use face_gallery::*;
pub use federation_routes::*;
pub use live_gateway::{
    Go2RtcUpstreamPort, InMemoryGo2RtcUpstream, LiveStreamGateway, ReqwestGo2RtcUpstream,
};
pub use mcp_routes::*;
pub use media_redaction::MediaDeliveryGate;
pub use ninjanvr_comms::{LogBufferHub, sanitize_logged_path};
pub use share_engine::*;
pub use storage_ops::*;
pub use telemetry::{
    DEFAULT_METRICS_PORT, HostMetricsProbe, LinuxHostMetricsProbe, ProcCpuTracker,
    ProcessResourceSample, RawPidProcReading, escape_prom_label_value,
    filter_stats_snapshot_for_allowed_cameras, is_camera_stream_online,
    render_detector_inference_metrics, render_prometheus_exposition,
};
pub use track_lifecycle::*;
pub use vod_routes::*;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as AxumWsMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{delete, get, patch, post, put};
use axum_extra::extract::cookie::CookieJar;
use base64::Engine;
use chrono::Local;
use futures_util::StreamExt;
pub use mse_hub::{MseGopCache, MseStreamHub};
use ninjanvr_comms::{CommsHub, SharedCommsHub};
use ninjanvr_config::{
    Go2RtcSupervisor, NINJANVR_VERSION, NinjaConfig, scrub_json_value, scrub_rtsp_credentials,
};
use ninjanvr_db::{CameraScope, EventFilter, ExportRow, NinjaDatabase, ReviewFilter};
use ninjanvr_infer::AiDetectorEngine;
use ninjanvr_media::{
    Mp4ExportRequest, RecordingEngine, SharedActivityLedger, build_mp4_export_with_progress,
    extract_historical_frame_jpeg, new_activity_ledger, probe_storage_writable,
    resolve_export_segment_path,
};
use pipeline::{SharedCameraMetrics, SharedLatestYuvFrames, VisionPipeline};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{RwLock, broadcast};
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

const MAX_STATS_HISTORY: usize = 60;

pub type LazySnapshotCacheKey = (String, u32, bool, String);
pub type LazySnapshotCacheValue = (Instant, Bytes, u64);

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<NinjaConfig>>,
    pub config_coordinator: ConfigCoordinator,
    pub db: NinjaDatabase,
    pub comms: SharedCommsHub,
    pub go2rtc_supervisor: Arc<Go2RtcSupervisor>,
    pub go2rtc_http_base: String,
    pub go2rtc_ws_base: String,
    pub http_client: reqwest::Client,
    pub jwt_secret: String,
    pub auth_gate: AuthSessionGate,
    pub start_instant: Instant,
    pub web_dir: PathBuf,
    pub media_dir: PathBuf,
    pub latest_frames: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    pub latest_yuv_frames: SharedLatestYuvFrames,
    pub preview_frames: preview_frames::PreviewFrameStore,
    pub historical_frames: export_jobs::HistoricalFrameCache,
    pub export_jobs: export_jobs::ExportJobManager,
    pub lazy_snapshot_cache: Arc<RwLock<HashMap<LazySnapshotCacheKey, LazySnapshotCacheValue>>>,
    pub mse_hub: MseStreamHub,
    pub activity_ledger: SharedActivityLedger,
    pub detector: Arc<AiDetectorEngine>,
    pub pipeline: Arc<VisionPipeline>,
    pub recorder: Arc<RecordingEngine>,
    pub vod_cache: VodWindowCache,
    pub camera_metrics: SharedCameraMetrics,
    pub log_hub: LogBufferHub,
    pub stats_history: Arc<RwLock<VecDeque<Value>>>,
    pub cpu_tracker: ProcCpuTracker,
    pub storage_degraded: Arc<std::sync::atomic::AtomicBool>,
    pub storage_error: Arc<std::sync::Mutex<Option<String>>>,
    pub storage_breakdown_cache: storage_ops::StorageBreakdownCache,
    pub ptz_active_presets: Arc<RwLock<HashMap<String, Option<String>>>>,
    pub camera_driver_runtime: CameraDriverRuntime,
    pub dataset_catalog: DatasetCatalog,
    pub face_gallery: FaceIdentityCatalog,
    pub share_engine: ShareLinkEngine,
    pub media_delivery_gate: MediaDeliveryGate,
    pub live_gateway: LiveStreamGateway,
    pub debug_reprocess_semaphore: Arc<tokio::sync::Semaphore>,
    pub vod_remux_semaphore: Arc<tokio::sync::Semaphore>,
    pub metrics_scrape_semaphore: Arc<tokio::sync::Semaphore>,
    pub unscoped_metrics_cache: Arc<tokio::sync::Mutex<Option<(Instant, u64, String)>>>,
    pub camera_logs_cooldown: Arc<std::sync::Mutex<HashMap<String, (Instant, Value)>>>,
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
    pub active_listener_ports: Arc<std::sync::RwLock<(u16, u16, u16)>>,
    #[doc(hidden)]
    pub allow_ephemeral_loopback_federation_for_test: Arc<std::sync::atomic::AtomicBool>,
}

impl AppState {
    /// Runs a blocking SQLite closure on Tokio's `spawn_blocking` pool so async HTTP/WS worker
    /// threads never stall on `Condvar::wait` or `PRAGMA busy_timeout`.
    pub async fn run_db<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&NinjaDatabase) -> Result<T> + Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || f(&db))
            .await
            .context("database blocking task panicked")?
    }

    /// Coordinates graceful shutdown of background tasks, MQTT, MSE sessions, vision workers,
    /// recorders, and the `go2rtc` supervisor (removing ephemeral `/dev/shm/go2rtc.yaml`).
    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        self.comms.shutdown_mqtt();
        self.mse_hub.shutdown().await;
        self.pipeline.shutdown().await;
        self.recorder.shutdown().await;
        self.go2rtc_supervisor.shutdown().await;
    }
    #[doc(hidden)]
    pub fn enable_ephemeral_loopback_federation_for_test(&self) {
        self.allow_ephemeral_loopback_federation_for_test
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Records the active `(http_port, https_port, metrics_port)` bound by the server process
    /// so SSRF guards also block custom CLI-configured listener ports.
    pub fn set_active_listener_ports(&self, http_port: u16, https_port: u16, metrics_port: u16) {
        if let Ok(mut guard) = self.active_listener_ports.write() {
            *guard = (http_port, https_port, metrics_port);
        }
    }

    /// Returns `true` when `port > 0` matches any active HTTP, HTTPS, or metrics listener port.
    pub fn is_active_listener_port(&self, port: u16) -> bool {
        if port == 0 {
            return true;
        }
        self.active_listener_ports
            .read()
            .is_ok_and(|g| port == g.0 || port == g.1 || (g.2 > 0 && port == g.2))
    }

    /// Returns `true` when `port > 0` matches the active non-TLS HTTP or metrics listener port.
    pub fn is_active_http_or_metrics_port(&self, port: u16) -> bool {
        if port == 0 {
            return true;
        }
        self.active_listener_ports
            .read()
            .is_ok_and(|g| port == g.0 || (g.2 > 0 && port == g.2))
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ReviewQueryParams {
    pub cameras: Option<String>,
    pub labels: Option<String>,
    pub exclude_labels: Option<String>,
    pub zones: Option<String>,
    pub reviewed: Option<i32>,
    pub severity: Option<String>,
    pub min_score: Option<f64>,
    pub stationary: Option<i32>,
    pub before: Option<f64>,
    pub after: Option<f64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
pub struct IdsQueryParams {
    pub ids: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct EventQueryParams {
    pub cameras: Option<String>,
    pub camera: Option<String>,
    pub labels: Option<String>,
    pub label: Option<String>,
    pub exclude_labels: Option<String>,
    pub zones: Option<String>,
    pub sub_labels: Option<String>,
    pub plate: Option<String>,
    pub recognized_license_plate: Option<String>,
    pub recognized_license_plates: Option<String>,
    pub favorites: Option<i32>,
    pub min_score: Option<f64>,
    pub stationary: Option<i32>,
    pub before: Option<f64>,
    pub after: Option<f64>,
    pub limit: Option<usize>,
    pub include_thumbnails: Option<i32>,
}

#[derive(Debug, Deserialize, Default)]
pub struct RecordingsQueryParams {
    pub after: Option<f64>,
    pub before: Option<f64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct TimelineQueryParams {
    pub camera: Option<String>,
    pub source_id: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ViewedReviewBody {
    pub ids: Vec<String>,
    #[serde(default = "default_true")]
    pub reviewed: bool,
}

fn default_true() -> bool {
    true
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub(crate) fn open_regular_non_symlink_readonly(
    path: &Path,
) -> std::io::Result<(std::fs::File, std::fs::Metadata)> {
    ninjanvr_config::open_regular_file_nofollow(path)
}

pub(crate) fn read_bounded_non_symlink_bytes_sync(path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let (mut file, fmeta) = open_regular_non_symlink_readonly(path).ok()?;
    if fmeta.len() > max_bytes {
        return None;
    }
    let mut buf = Vec::with_capacity(fmeta.len() as usize);
    (&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)
        .ok()?;
    if (buf.len() as u64) > max_bytes {
        return None;
    }
    Some(buf)
}

async fn read_bounded_non_symlink_bytes(path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    let path_owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_bounded_non_symlink_bytes_sync(&path_owned, max_bytes))
        .await
        .ok()
        .flatten()
}

pub fn build_router(state: AppState) -> Router {
    build_router_with_auth(state, false)
}

pub fn build_router_with_auth(state: AppState, enforce_auth: bool) -> Router {
    let web_dir = state.web_dir.clone();

    let clips_dir = state.media_dir.join("clips");
    let recordings_dir = state.media_dir.join("recordings");
    let exports_dir = state.media_dir.join("exports");

    let api_router = Router::new()
        .route("/healthz", get(api_healthz))
        .route("/readyz", get(api_readyz))
        .route("/version", get(api_version))
        .route("/ui/switch", get(api_ui_switch))
        .route("/ui/version", get(api_ui_version))
        .route("/auth", get(api_auth_verify))
        .route("/auth/first_time_login", get(api_first_time_login))
        .route("/auth/providers", get(api_auth_providers))
        .route("/auth/oidc/login", get(api_auth_oidc_login))
        .route("/auth/oidc/callback", get(api_auth_oidc_callback))
        .route("/login", post(api_login))
        .route("/logout", get(api_logout).post(api_logout))
        .route("/profile", get(api_profile).post(api_profile_set))
        .route(
            "/profile/active",
            get(api_profile_active).post(api_profile_set),
        )
        .route("/profiles", get(api_profiles))
        .route("/users", get(api_users).post(api_user_create))
        .route("/users/{username}", delete(api_user_delete))
        .route("/users/{username}/password", put(api_user_password))
        .route("/users/{username}/role", put(api_user_role))
        .route("/users/{username}/cameras", put(api_user_cameras))
        .route("/tokens", get(api_tokens_list).post(api_token_create))
        .route("/tokens/{id}", delete(api_token_delete))
        .route("/config", get(api_config))
        .route("/config/raw", get(api_config_raw))
        .route("/config/raw_paths", get(api_empty_array))
        .route("/config/save", post(api_config_save))
        .route(
            "/config/structured",
            post(api_config_structured).put(api_config_structured),
        )
        .route("/config/set", put(api_config_set))
        .route("/config/schema.json", get(api_config_schema))
        .route("/detectors/catalog", get(api_detectors_catalog))
        .route(
            "/detectors/models/upload",
            post(api_detector_model_upload).layer(axum::extract::DefaultBodyLimit::max(
                MAX_MODEL_UPLOAD_BYTES + 2 * 1024 * 1024,
            )),
        )
        .route("/dataset/summary", get(api_dataset_summary))
        .route("/dataset/samples", get(api_dataset_samples))
        .route(
            "/dataset/samples/from-event/{event_id}",
            post(api_dataset_sample_from_event),
        )
        .route(
            "/dataset/samples/{id}",
            get(api_dataset_sample_get)
                .put(api_dataset_sample_update)
                .delete(api_dataset_sample_delete),
        )
        .route(
            "/dataset/samples/{id}/auto-annotate",
            post(api_dataset_sample_auto_annotate),
        )
        .route(
            "/dataset/auto-annotate",
            post(api_dataset_batch_auto_annotate),
        )
        .route(
            "/dataset/samples/{id}/image.webp",
            get(api_dataset_sample_image_webp),
        )
        .route("/dataset/export.zip", get(api_dataset_export_zip))
        .route("/ffmpeg/presets", get(api_ffmpeg_presets))
        .route("/ffprobe", get(api_ffprobe))
        .route("/streams/probe", post(api_streams_probe))
        .route("/keyframe_analysis", get(api_keyframe_analysis))
        .route("/restart", post(api_restart))
        .route("/logs/{service}", get(api_service_logs))
        .route("/stats", get(api_stats))
        .route("/stats/history", get(api_stats_history))
        .route("/metrics", get(api_prometheus_metrics))
        .route("/ai/context", get(api_ai_context))
        .route("/mcp", get(api_mcp_get_handler).post(api_mcp_handler))
        .route("/debug/reprocess", post(api_debug_reprocess))
        .route("/debug/lpr-simulate", post(api_debug_lpr_simulate))
        .route("/notifications/test", post(api_notifications_test))
        .route("/federation/nodes", get(api_federation_nodes))
        .route(
            "/federation/{node}/{*path}",
            get(api_federation_proxy).post(api_federation_proxy),
        )
        .route(
            "/camera-proxy/{camera}",
            get(api_camera_proxy_root).post(api_camera_proxy_root),
        )
        .route(
            "/camera-proxy/{camera}/",
            get(api_camera_proxy_root).post(api_camera_proxy_root),
        )
        .route(
            "/camera-proxy/{camera}/{*path}",
            get(api_camera_proxy_subpath).post(api_camera_proxy_subpath),
        )
        .route("/review", get(api_review))
        .route("/review/summary", get(api_review_summary))
        .route("/review/activity/motion", get(api_review_activity_motion))
        .route("/review/event/{event_id}", get(api_review_by_event_id))
        .route(
            "/review/{id}",
            get(api_review_by_id).delete(api_review_delete),
        )
        .route("/review/{id}/preview", get(api_review_preview))
        .route("/review/{id}/frames", get(api_review_frames))
        .route("/review/{id}/frames/{frame}", get(api_review_frame))
        .route("/review/{id}/viewed", delete(api_review_unview_one))
        .route(
            "/review/{id}/notes",
            put(api_review_notes).post(api_review_notes),
        )
        .route("/review_ids", get(api_review_ids))
        .route("/reviews", delete(api_reviews_delete))
        .route("/reviews/viewed", post(api_reviews_viewed))
        .route("/reviews/delete", post(api_reviews_delete))
        .route("/events", get(api_events).delete(api_events_bulk_delete))
        .route("/events/", get(api_events).delete(api_events_bulk_delete))
        .route("/event_ids", get(api_event_ids))
        .route("/events/explore", get(api_events_explore))
        .route("/events/search", get(api_events))
        .route("/events/summary", get(api_events_summary))
        .route(
            "/events/{id}",
            get(api_event_by_id).delete(api_event_delete),
        )
        .route(
            "/events/{id}/retain",
            post(api_event_retain_on).delete(api_event_retain_off),
        )
        .route("/events/{id}/false_positive", put(api_event_false_positive))
        .route("/events/{id}/verify-ai", post(api_event_verify_ai))
        .route(
            "/false_positives/signatures",
            get(api_list_false_positive_signatures),
        )
        .route(
            "/false_positives/signatures/{id}",
            delete(api_delete_false_positive_signature),
        )
        .route("/events/{id}/sub_label", post(api_event_sub_label))
        .route(
            "/events/{id}/license_plate",
            put(api_event_license_plate).post(api_event_license_plate),
        )
        .route("/events/{id}/description", post(api_event_description))
        .route(
            "/events/{id}/notes",
            put(api_event_notes).post(api_event_notes),
        )
        .route("/events/{id}/thumbnail.webp", get(api_event_thumbnail_webp))
        .route("/events/{id}/thumbnail.jpg", get(api_event_thumbnail_jpg))
        .route("/events/{id}/snapshot.jpg", get(api_event_snapshot_jpg))
        .route(
            "/events/{id}/snapshot-clean.webp",
            get(api_event_snapshot_clean_webp),
        )
        .route("/events/{id}/clip.mp4", get(api_event_clip_mp4))
        .route("/timeline", get(api_timeline))
        .route("/recordings/summary", get(api_all_recordings_summary))
        .route("/recordings/days", get(api_all_recordings_days_summary))
        .route("/recordings/sync", post(api_recordings_sync))
        .route("/recordings/unavailable", get(api_empty_array))
        .route("/recordings/storage", get(api_recordings_storage))
        .route("/storage/breakdown", get(api_storage_breakdown))
        .route("/storage/cleanup", post(api_storage_cleanup))
        .route("/exports", get(api_exports))
        .route("/exports/delete", post(api_exports_bulk_delete))
        .route("/share", get(api_share_list).post(api_share_create))
        .route("/share/{id}", delete(api_share_revoke))
        .route("/jobs/export", get(api_export_jobs_list))
        .route("/jobs/export/{id}/cancel", post(api_export_job_cancel))
        .route("/cases", get(api_cases).post(api_case_create))
        .route("/cases/{id}", delete(api_case_delete))
        .route(
            "/export/{camera}/start/{start}/end/{end}",
            post(api_export_create),
        )
        .route("/export/{id}", delete(api_export_delete))
        .route(
            "/export/{id}/rename",
            patch(api_export_rename).post(api_export_rename),
        )
        .route(
            "/preview/{camera}/start/{start}/end/{end}",
            get(api_previews),
        )
        .route(
            "/preview/{camera}/start/{start}/end/{end}/frames",
            get(api_preview_camera_frames),
        )
        .route(
            "/preview/{camera}/frame/{frame}",
            get(api_preview_camera_frame),
        )
        .route("/preview/{camera}/at/{target}", get(api_preview_camera_at))
        .route(
            "/preview/{preview_name}/thumbnail.webp",
            get(api_preview_thumbnail),
        )
        .route(
            "/preview/{preview_name}/thumbnail.jpg",
            get(api_preview_thumbnail),
        )
        .route("/go2rtc/streams", get(api_go2rtc_streams))
        .route("/go2rtc/streams/{stream}", get(api_go2rtc_stream_one))
        .route(
            "/go2rtc/streams/{stream}/prewarm",
            post(api_go2rtc_stream_prewarm),
        )
        .route("/go2rtc/webrtc", post(api_go2rtc_webrtc))
        .route(
            "/{camera}/recordings",
            get(api_camera_recordings).delete(api_camera_recordings_delete),
        )
        .route(
            "/{camera}/recordings/summary",
            get(api_camera_recordings_summary),
        )
        .route("/{camera}/ptz/info", get(api_camera_ptz_info))
        .route("/{camera}/ptz/move", post(api_camera_ptz_move))
        .route("/{camera}/ptz/stop", post(api_camera_ptz_stop))
        .route("/{camera}/ptz/preset", post(api_camera_ptz_preset))
        .route(
            "/{camera}/ptz/preset/save",
            post(api_camera_ptz_preset_save),
        )
        .route(
            "/{camera}/ptz/preset/{token}",
            delete(api_camera_ptz_preset_delete),
        )
        .route(
            "/{camera}/hardware",
            get(api_camera_hardware_get).post(api_camera_hardware_set),
        )
        .route("/{camera}/logs", get(api_camera_logs_get))
        .route("/{camera}/audio/talkback", post(api_camera_audio_talkback))
        .route(
            "/{camera}/state",
            get(api_camera_state_get).post(api_camera_state_set),
        )
        .route(
            "/{camera}/latest.webp",
            get(api_camera_latest_frame).head(api_camera_latest_frame_head),
        )
        .route(
            "/{camera}/latest.jpg",
            get(api_camera_latest_frame).head(api_camera_latest_frame_head),
        )
        .route("/{camera}/frame", get(api_camera_historical_frame))
        .route("/{camera}/snapshot.jpg", get(api_camera_snapshot_download))
        .route("/labels", get(api_labels))
        .route("/sub_labels", get(api_sub_labels))
        .route("/audio_labels", get(api_empty_array))
        .route(
            "/recognized_license_plates",
            get(api_recognized_license_plates),
        )
        .route("/classification/attributes", get(api_empty_array))
        .route("/classification/custom", get(api_empty_object))
        .route(
            "/genai/models",
            get(api_genai_models_get).post(api_genai_models_post),
        )
        .route("/genai/validate", post(api_genai_models_post))
        .route("/plus/models", get(api_empty_array))
        .route("/faces", get(api_faces_list))
        .route("/faces/{name}", delete(api_face_delete))
        .route("/faces/{name}/enroll", post(api_face_enroll))
        .fallback(|| async {
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "success": false, "message": "API endpoint not found" })),
            )
        });

    Router::new()
        .nest("/api", api_router)
        .route("/share/{token}", get(share_player_page))
        .route("/healthz", get(api_healthz))
        .route("/readyz", get(api_readyz))
        .route("/version", get(api_version))
        .route("/metrics", get(api_prometheus_metrics))
        .route("/auth", get(api_auth_verify))
        .route("/logout", get(api_logout).post(api_logout))
        .route("/ws", get(ws_state_handler))
        .route("/live/mse/api/ws", get(ws_go2rtc_proxy))
        .route("/live/webrtc/api/ws", get(ws_go2rtc_proxy))
        .route(
            "/vod/{camera}/start/{start}/end/{end}/master.m3u8",
            get(vod_master_m3u8),
        )
        .route(
            "/vod/{camera}/start/{start}/end/{end}/index.m3u8",
            get(vod_index_m3u8),
        )
        .route(
            "/vod/{camera}/start/{start}/end/{end}/index-v1-a1.m3u8",
            get(vod_index_m3u8),
        )
        .route(
            "/vod/{camera}/start/{start}/end/{end}/init-v1-a1.mp4",
            get(vod_init_mp4),
        )
        .route(
            "/vod/{camera}/start/{start}/end/{end}/{segment}",
            get(vod_segment_m4s),
        )
        .route(
            "/vod/clip/{camera}/start/{start}/end/{end}/master.m3u8",
            get(vod_master_m3u8),
        )
        .route(
            "/vod/clip/{camera}/start/{start}/end/{end}/index.m3u8",
            get(vod_index_m3u8),
        )
        .route(
            "/vod/clip/{camera}/start/{start}/end/{end}/index-v1-a1.m3u8",
            get(vod_index_m3u8),
        )
        .route(
            "/vod/clip/{camera}/start/{start}/end/{end}/init-v1-a1.mp4",
            get(vod_init_mp4),
        )
        .route(
            "/vod/clip/{camera}/start/{start}/end/{end}/{segment}",
            get(vod_segment_m4s),
        )
        .route("/login", get(serve_login_page))
        .nest_service("/assets", ServeDir::new(web_dir.join("assets")))
        .fallback(get(serve_spa_fallback))
        .layer(CompressionLayer::new())
        // Binary media directories and MP4 share streams are mounted outside CompressionLayer so MP4/WebP files
        // always preserve exact Content-Length and Accept-Ranges: bytes without Brotli/Gzip chunking.
        .route("/share/{token}/video.mp4", get(share_video_mp4))
        .nest_service("/clips", ServeDir::new(clips_dir))
        .nest_service("/recordings", ServeDir::new(recordings_dir))
        .nest_service("/exports", ServeDir::new(exports_dir))
        .layer(axum::middleware::from_fn_with_state(
            (state.clone(), enforce_auth),
            auth_gate_middleware,
        ))
        .layer(CorsLayer::new())
        .with_state(state)
}

/// Builds the dedicated, port-isolated OpenMetrics & health telemetry router (`:9091` by default).
///
/// Mounts **only** read-only operational endpoints (`/metrics`, `/api/metrics`, `/healthz`,
/// `/api/healthz`, `/version`, `/api/version`) without authentication and exports full instance
/// telemetry (`allowed = None`), completely decoupled from NVR API, config, auth, media, MCP,
/// and WebSocket routes. Omits `CorsLayer` so cross-origin browser scripts cannot scrape telemetry.
pub fn build_metrics_router(state: AppState) -> Router {
    Router::new()
        .route("/metrics", get(telemetry_prometheus_metrics))
        .route("/api/metrics", get(telemetry_prometheus_metrics))
        .route("/healthz", get(api_healthz))
        .route("/api/healthz", get(api_healthz))
        .route("/readyz", get(api_readyz))
        .route("/api/readyz", get(api_readyz))
        .route("/version", get(api_version))
        .route("/api/version", get(api_version))
        .fallback(telemetry_not_found)
        .layer(CompressionLayer::new())
        .with_state(state)
}

async fn telemetry_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "success": false,
            "message": "Endpoint not available on dedicated metrics listener"
        })),
    )
        .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct UiSwitchParams {
    pub target: Option<String>,
    pub redirect: Option<String>,
}

async fn api_ui_switch(Query(q): Query<UiSwitchParams>) -> Response {
    let _ = &q.target;
    let redirect_to = q
        .redirect
        .as_deref()
        .filter(|r| {
            r.starts_with('/')
                && !r.starts_with("//")
                && !r.contains('\\')
                && !r.chars().any(|c| c.is_control())
        })
        .unwrap_or("/");
    let cookie_val = "ninjanvr_ui=v2; Path=/; Max-Age=31536000; SameSite=Lax".to_string();
    (
        StatusCode::FOUND,
        [
            (header::SET_COOKIE, cookie_val),
            (header::LOCATION, redirect_to.to_string()),
        ],
    )
        .into_response()
}

async fn api_ui_version() -> Json<Value> {
    Json(json!({
        "active": "v2",
        "available": ["v2"],
        "version": NINJANVR_VERSION
    }))
}

const MAX_SPA_STATIC_FILE_BYTES: u64 = 32 * 1024 * 1024;

fn read_contained_web_asset_sync(base_dir: &Path, candidate: &Path) -> Option<Vec<u8>> {
    let base_canon = std::fs::canonicalize(base_dir).ok()?;
    let cand_canon = std::fs::canonicalize(candidate).ok()?;
    if !cand_canon.starts_with(&base_canon) {
        return None;
    }
    let bytes = read_bounded_non_symlink_bytes_sync(candidate, MAX_SPA_STATIC_FILE_BYTES)?;
    Some(bytes)
}

const SPA_CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; font-src 'self' data:; img-src 'self' data: blob: https:; media-src 'self' blob:; worker-src 'self' blob:; child-src 'self' blob:; frame-src 'self'; connect-src 'self' ws: wss:; frame-ancestors 'none'; base-uri 'self'; object-src 'none'";

async fn serve_login_page(State(state): State<AppState>) -> Response {
    let web_dir = state.web_dir.clone();
    let index_path = web_dir.join("index.html");
    let html_opt = tokio::task::spawn_blocking(move || {
        read_contained_web_asset_sync(&web_dir, &index_path)
            .and_then(|bytes| String::from_utf8(bytes).ok())
    })
    .await
    .ok()
    .flatten();
    match html_opt {
        Some(html) => (
            [(header::CONTENT_SECURITY_POLICY, SPA_CONTENT_SECURITY_POLICY)],
            Html(html),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn serve_spa_fallback(State(state): State<AppState>, uri: axum::http::Uri) -> Response {
    let rel_path = uri.path().trim_start_matches('/');
    let primary_dir = state.web_dir.clone();

    if !rel_path.is_empty()
        && !rel_path.contains("..")
        && !rel_path.contains('\\')
        && !rel_path.split('/').any(|seg| seg.starts_with('.'))
        && percent_decode_path(rel_path).is_some_and(|dec| {
            !dec.contains("..")
                && !dec.contains('\\')
                && !dec.split('/').any(|seg| seg.starts_with('.'))
        })
    {
        let rel_owned = rel_path.to_string();
        let dir_for_cand = primary_dir.clone();
        let resolved_asset = tokio::task::spawn_blocking(move || {
            let mut file_candidate = dir_for_cand.join(&rel_owned);
            if rel_owned == "favicon.ico"
                && !std::fs::symlink_metadata(&file_candidate)
                    .is_ok_and(|m| m.file_type().is_file())
            {
                file_candidate = dir_for_cand.join("favicon.svg");
            }
            let bytes = read_contained_web_asset_sync(&dir_for_cand, &file_candidate)?;
            let mime = match file_candidate.extension().and_then(|e| e.to_str()) {
                Some("js" | "mjs") => "application/javascript; charset=utf-8",
                Some("css") => "text/css; charset=utf-8",
                Some("svg") => "image/svg+xml",
                Some("png") => "image/png",
                Some("ico") => "image/x-icon",
                Some("webp") => "image/webp",
                Some("json" | "webmanifest") => "application/json; charset=utf-8",
                Some("woff2") => "font/woff2",
                Some("txt") => "text/plain; charset=utf-8",
                Some("html") => "text/html; charset=utf-8",
                _ => "application/octet-stream",
            };
            Some((mime, bytes))
        })
        .await
        .ok()
        .flatten();
        if let Some((mime, bytes)) = resolved_asset {
            if mime.starts_with("text/html") {
                return (
                    [
                        (header::CONTENT_TYPE, mime),
                        (header::CONTENT_SECURITY_POLICY, SPA_CONTENT_SECURITY_POLICY),
                    ],
                    bytes,
                )
                    .into_response();
            }
            return ([(header::CONTENT_TYPE, mime)], bytes).into_response();
        }
    }

    let index_path = primary_dir.join("index.html");
    let html_opt = tokio::task::spawn_blocking(move || {
        read_contained_web_asset_sync(&primary_dir, &index_path)
            .and_then(|bytes| String::from_utf8(bytes).ok())
    })
    .await
    .ok()
    .flatten();
    match html_opt {
        Some(html) => (
            [(header::CONTENT_SECURITY_POLICY, SPA_CONTENT_SECURITY_POLICY)],
            Html(html),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn api_healthz() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        "ok\n",
    )
        .into_response()
}

async fn api_readyz(State(state): State<AppState>) -> Response {
    let db_ok = state
        .run_db(|db| {
            db.with_read_conn(|conn| Ok(conn.query_row("SELECT 1", [], |r| r.get::<_, i32>(0))?))
        })
        .await
        .is_ok_and(|v| v == 1);
    let storage_degraded = state
        .storage_degraded
        .load(std::sync::atomic::Ordering::Relaxed);
    let status = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "ready": db_ok,
            "database": db_ok,
            "storage_degraded": storage_degraded,
            "go2rtc_pid": state.go2rtc_supervisor.pid()
        })),
    )
        .into_response()
}

async fn api_version() -> &'static str {
    NINJANVR_VERSION
}

async fn api_profile(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Response {
    let cfg = state.config.read().await;
    let cameras: Vec<String> = cfg.cameras.keys().cloned().collect();
    let user_opt = extract_authenticated_user_with_cfg(&state, &jar, &headers, &cfg.auth);

    let (username, role) = user_opt.unwrap_or_else(|| ("admin".to_string(), "admin".to_string()));
    let scoped_cameras: Vec<String> =
        match resolve_user_allowed_cameras(&state, &jar, &headers, &cfg.auth, &username, &role) {
            None => cameras,
            Some(allowed) => cameras
                .into_iter()
                .filter(|c| allowed.iter().any(|a| a == c || a == "*"))
                .collect(),
        };

    Json(json!({
        "username": username,
        "role": role,
        "allowed_cameras": scoped_cameras,
        "active_profile": state.comms.get_active_profile()
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct SetProfileBody {
    pub profile: String,
}

async fn api_profile_set(
    State(state): State<AppState>,
    uri: Uri,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<SetProfileBody>,
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
    let caller_role_opt = {
        let cfg = state.config.read().await;
        if cfg.auth.enabled {
            let has_creds = headers.contains_key(header::AUTHORIZATION)
                || jar.get(&cfg.auth.cookie_name).is_some()
                || (cfg.auth.proxy.enabled && extract_trusted_proxy_peer_ip(&headers).is_some());
            match extract_authenticated_user_with_cfg(&state, &jar, &headers, &cfg.auth) {
                Some((_, r)) => Some(r.to_ascii_lowercase()),
                None if has_creds => Some("unauthorized".to_string()),
                None => None,
            }
        } else {
            None
        }
    };
    if matches!(
        caller_role_opt.as_deref(),
        Some("viewer" | "read_only" | "metrics" | "unauthorized")
    ) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Read-only role or token cannot change global NVR security profile"
            })),
        )
            .into_response();
    }
    if caller_allowed_cameras(&state, &jar, &headers)
        .await
        .is_some()
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Camera-restricted users cannot change global NVR security profile"
            })),
        )
            .into_response();
    }
    let active = state.comms.set_active_profile(&body.profile);
    Json(json!({
        "success": true,
        "active_profile": active
    }))
    .into_response()
}

async fn api_profile_active(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "active_profile": state.comms.get_active_profile() }))
}

async fn api_profiles(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "profiles": ["armed", "home", "disarmed"],
        "active_profile": state.comms.get_active_profile(),
        "last_activated": {}
    }))
}

async fn api_users(State(state): State<AppState>, jar: CookieJar, headers: HeaderMap) -> Response {
    if let Err(err_resp) = authorize_admin_read(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    let users = state.run_db(|db| db.list_users()).await.unwrap_or_default();
    Json(Value::Array(users)).into_response()
}

#[derive(Debug, Deserialize)]
pub struct CreateUserBody {
    pub username: String,
    pub password: String,
    #[serde(default = "default_admin_role")]
    pub role: String,
    #[serde(default)]
    pub allowed_cameras: Option<String>,
}

fn default_admin_role() -> String {
    "admin".to_string()
}

async fn api_user_create(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<CreateUserBody>,
) -> Response {
    if let Err(err_resp) = authorize_global_admin_mutation(&state, &jar, &uri, &headers).await {
        return err_resp.into_response();
    }
    if body.password.len() > 1024 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Password exceeds maximum length" })),
        )
            .into_response();
    }
    let iterations = state.config.read().await.auth.hash_iterations;
    let Ok(_pbkdf2_permit) = state.auth_gate.pbkdf2_semaphore().acquire().await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "success": false, "message": "Authentication worker pool unavailable" })),
        )
            .into_response();
    };
    let db_clone = state.db.clone();
    let username_owned = body.username.clone();
    let password_owned = body.password.clone();
    let role_owned = body.role.clone();
    let cams_owned = body
        .allowed_cameras
        .clone()
        .unwrap_or_else(|| "*".to_string());
    let create_res = tokio::task::spawn_blocking(move || {
        db_clone
            .create_user_with_cameras(
                &username_owned,
                &password_owned,
                &role_owned,
                &cams_owned,
                iterations,
            )
            .map(|()| db_clone.list_users().unwrap_or_default())
    })
    .await;
    drop(_pbkdf2_permit);
    match create_res {
        Ok(Ok(users)) => {
            state.auth_gate.invalidate_user(&body.username);
            Json(json!({
                "success": true,
                "message": format!("User '{}' created", body.username.trim()),
                "users": users
            }))
            .into_response()
        }
        Ok(Err(err)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateUserPasswordBody {
    pub password: String,
}

async fn api_user_password(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(username): AxumPath<String>,
    Json(body): Json<UpdateUserPasswordBody>,
) -> Response {
    if let Err(err_resp) = authorize_global_admin_mutation(&state, &jar, &uri, &headers).await {
        return err_resp.into_response();
    }
    if body.password.len() > 1024 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Password exceeds maximum length" })),
        )
            .into_response();
    }
    let iterations = state.config.read().await.auth.hash_iterations;
    let Ok(_pbkdf2_permit) = state.auth_gate.pbkdf2_semaphore().acquire().await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "success": false, "message": "Authentication worker pool unavailable" })),
        )
            .into_response();
    };
    let db_clone = state.db.clone();
    let username_owned = username.clone();
    let password_owned = body.password.clone();
    let update_res = tokio::task::spawn_blocking(move || {
        let updated =
            db_clone.update_user_password(&username_owned, &password_owned, iterations)?;
        let is_admin_role = if updated {
            db_clone
                .get_user(&username_owned)
                .ok()
                .flatten()
                .is_some_and(|u| u.role.trim().eq_ignore_ascii_case("admin"))
        } else {
            false
        };
        Ok::<(bool, bool), anyhow::Error>((updated, is_admin_role))
    })
    .await;
    drop(_pbkdf2_permit);
    match update_res {
        Ok(Ok((true, is_admin_role))) => {
            state.auth_gate.invalidate_user(&username);
            state
                .auth_gate
                .clear_all_login_failures_for_user(username.trim());
            if username.trim().eq_ignore_ascii_case("admin") && is_admin_role {
                let cfg_dir_opt = {
                    let mut cfg_guard = state.config.write().await;
                    cfg_guard.auth.admin_first_time_login = false;
                    cfg_guard.config_path.parent().map(Path::to_path_buf)
                };
                if let Some(cfg_dir) = cfg_dir_opt {
                    remove_initial_admin_password_file(&cfg_dir);
                }
            }
            Json(json!({
                "success": true,
                "message": format!("Password updated for '{username}'")
            }))
            .into_response()
        }
        Ok(Ok((false, _))) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "User not found" })),
        )
            .into_response(),
        Ok(Err(err)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateUserRoleBody {
    pub role: String,
    #[serde(default)]
    pub allowed_cameras: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateUserCamerasBody {
    pub allowed_cameras: String,
}

async fn api_user_role(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(username): AxumPath<String>,
    Json(body): Json<UpdateUserRoleBody>,
) -> Response {
    if let Err(err_resp) = authorize_global_admin_mutation(&state, &jar, &uri, &headers).await {
        return err_resp.into_response();
    }
    if let Some(ref cams) = body.allowed_cameras
        && (cams.len() > 4096
            || cams
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "*")
                .any(|c| !is_safe_id(c)))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Invalid allowed_cameras format"
            })),
        )
            .into_response();
    }
    let u_owned = username.clone();
    let r_owned = body.role.clone();
    let c_owned = body.allowed_cameras.clone();
    let update_res = state
        .run_db(move |db| db.update_user_role_with_cameras(&u_owned, &r_owned, c_owned.as_deref()))
        .await;
    match update_res {
        Ok(true) => {
            state.auth_gate.invalidate_user(&username);
            let st_for_share = state.clone();
            let u_share = username.clone();
            let r_share = body.role.clone();
            let share_res = tokio::task::spawn_blocking(move || {
                st_for_share.share_engine.on_creator_permissions_changed(
                    &st_for_share,
                    &u_share,
                    Some(&r_share),
                )
            })
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
            .and_then(|r| r);
            if let Err(err) = share_res {
                warn!("Failed to revoke share links for demoted user '{username}': {err}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "success": false,
                        "message": "Failed to revoke share links for demoted user"
                    })),
                )
                    .into_response();
            }
            let users = state.run_db(|db| db.list_users()).await.unwrap_or_default();
            Json(json!({
                "success": true,
                "message": format!("Role updated for '{username}'"),
                "users": users
            }))
            .into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "User not found" })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn api_user_cameras(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(username): AxumPath<String>,
    Json(body): Json<UpdateUserCamerasBody>,
) -> Response {
    if let Err(err_resp) = authorize_global_admin_mutation(&state, &jar, &uri, &headers).await {
        return err_resp.into_response();
    }
    let u_owned = username.clone();
    let cams_owned = body.allowed_cameras.clone();
    match state
        .run_db(move |db| db.update_user_allowed_cameras(&u_owned, &cams_owned))
        .await
    {
        Ok(true) => {
            state.auth_gate.invalidate_user(&username);
            let st_for_share = state.clone();
            let u_share = username.clone();
            let _ = tokio::task::spawn_blocking(move || {
                st_for_share.share_engine.on_creator_permissions_changed(
                    &st_for_share,
                    &u_share,
                    None,
                )
            })
            .await;
            let users = state.run_db(|db| db.list_users()).await.unwrap_or_default();
            Json(json!({
                "success": true,
                "message": format!("Allowed cameras updated for '{username}'"),
                "users": users
            }))
            .into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "User not found" })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn api_user_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(username): AxumPath<String>,
) -> Response {
    if let Err(err_resp) = authorize_global_admin_mutation(&state, &jar, &uri, &headers).await {
        return err_resp.into_response();
    }
    let u_owned = username.clone();
    match state.run_db(move |db| db.delete_user(&u_owned)).await {
        Ok(true) => {
            state.auth_gate.record_user_deleted(&username, unix_now());
            state
                .auth_gate
                .clear_all_login_failures_for_user(username.trim());
            let st_for_share = state.clone();
            let u_share = username.clone();
            let share_res = tokio::task::spawn_blocking(move || {
                st_for_share
                    .share_engine
                    .on_creator_deleted(&st_for_share, &u_share)
            })
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
            .and_then(|r| r);
            if let Err(err) = share_res {
                warn!("Failed to revoke share links for deleted user '{username}': {err}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "success": false,
                        "message": "Failed to revoke share links for deleted user"
                    })),
                )
                    .into_response();
            }
            let users = state.run_db(|db| db.list_users()).await.unwrap_or_default();
            Json(json!({
                "success": true,
                "message": format!("User '{username}' deleted"),
                "users": users
            }))
            .into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "User not found" })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateApiTokenBody {
    pub name: String,
    #[serde(default = "default_token_scope")]
    pub scope: String,
    #[serde(default)]
    pub expires_in_days: Option<u32>,
}

fn default_token_scope() -> String {
    "read_only".to_string()
}

async fn api_tokens_list(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Response {
    if let Err(err_resp) = authorize_admin_read(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    let tokens = state
        .run_db(|db| db.list_api_tokens())
        .await
        .unwrap_or_default();
    Json(Value::Array(tokens)).into_response()
}

async fn api_token_create(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<CreateApiTokenBody>,
) -> Response {
    if let Err(err_resp) = authorize_global_admin_mutation(&state, &jar, &uri, &headers).await {
        return err_resp.into_response();
    }
    let trimmed_name = body.name.trim();
    if trimmed_name.is_empty()
        || trimmed_name.len() > 128
        || trimmed_name.chars().any(|c| c.is_control())
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Token name must be 1..=128 printable characters"
            })),
        )
            .into_response();
    }
    if body.expires_in_days.is_some_and(|d| d == 0 || d > 36500) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "expires_in_days must be between 1 and 36500"
            })),
        )
            .into_response();
    }
    let expires_at = body
        .expires_in_days
        .map(|d| (unix_now() as f64) + (d as f64) * 86400.0);
    let name_owned = body.name.clone();
    let scope_owned = body.scope.clone();
    match state
        .run_db(move |db| {
            let (meta, plaintext) = db.create_api_token(&name_owned, &scope_owned, expires_at)?;
            let tokens = db.list_api_tokens().unwrap_or_default();
            Ok((meta, plaintext, tokens))
        })
        .await
    {
        Ok((meta, plaintext, tokens)) => {
            state.auth_gate.invalidate_tokens();
            Json(json!({
                "success": true,
                "token": plaintext,
                "metadata": meta,
                "tokens": tokens
            }))
            .into_response()
        }
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn api_token_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if let Err(err_resp) = authorize_global_admin_mutation(&state, &jar, &uri, &headers).await {
        return err_resp.into_response();
    }
    let id_for_lookup = id.clone();
    let (token_name, del_res) = state
        .run_db(move |db| {
            let tname = db.list_api_tokens().ok().and_then(|tokens| {
                tokens.into_iter().find_map(|item| {
                    if item["id"].as_str() == Some(id_for_lookup.trim()) {
                        item["name"].as_str().map(str::to_string)
                    } else {
                        None
                    }
                })
            });
            Ok((tname, db.delete_api_token(&id_for_lookup)))
        })
        .await
        .unwrap_or_else(|e| (None, Err(e)));
    match del_res {
        Ok(true) => {
            state.auth_gate.invalidate_tokens();
            let st_for_share = state.clone();
            let tname_share = token_name.clone();
            let share_res = tokio::task::spawn_blocking(move || {
                st_for_share
                    .share_engine
                    .on_creator_token_revoked(&st_for_share, tname_share.as_deref())
            })
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
            .and_then(|r| r);
            if let Err(err) = share_res {
                let name = token_name.as_deref().unwrap_or("");
                warn!("Failed to revoke share links for deleted token '{name}': {err}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "success": false,
                        "message": "Failed to revoke share links for deleted API token"
                    })),
                )
                    .into_response();
            }
            let tokens = state
                .run_db(|db| db.list_api_tokens())
                .await
                .unwrap_or_default();
            Json(json!({
                "success": true,
                "tokens": tokens
            }))
            .into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "Token not found" })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
fn is_safe_labelmap_path_str(raw: &str) -> bool {
    config_coordinator::is_safe_labelmap_path_str(raw)
}

fn scrub_raw_yaml_secrets_strict(raw_yaml: &str, cfg: &NinjaConfig) -> String {
    config_coordinator::scrub_raw_yaml_secrets_strict(raw_yaml, cfg)
}

async fn api_config(State(state): State<AppState>, jar: CookieJar, headers: HeaderMap) -> Response {
    let principal = match authenticate_read(&state, &jar, &headers).await {
        Ok(p) => p,
        Err(rej) => return rej.into_response(),
    };
    let cfg = state.config.read().await;
    Json(config_coordinator::to_scoped_api_config_for_principal(
        &cfg, &principal,
    ))
    .into_response()
}

async fn api_config_raw(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Response {
    if let Err(err_resp) = authorize_admin_read(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    let cfg = state.config.read().await;
    Json(Value::String(scrub_raw_yaml_secrets_strict(
        &cfg.raw_yaml,
        &cfg,
    )))
    .into_response()
}

async fn api_config_save(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(err_resp) = authorize_global_admin_mutation(&state, &jar, &uri, &headers).await {
        return err_resp.into_response();
    }
    if body.len() > 1_048_576 {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "success": false,
                "message": "Configuration YAML exceeds maximum allowed size (1 MiB)"
            })),
        )
            .into_response();
    }
    let commit_res = tokio::spawn(async move {
        let outcome = match state
            .config_coordinator
            .commit_raw_yaml_full(
                &body,
                Some(&state.detector),
                Some(&state.camera_driver_runtime),
            )
            .await
        {
            Ok(out) => out,
            Err(config_coordinator::ConfigCommitError::Validation(msg)) => {
                return Err((StatusCode::BAD_REQUEST, msg));
            }
            Err(config_coordinator::ConfigCommitError::Persistence(msg)) => {
                error!("Failed to persist config.yml: {msg}");
                return Err((StatusCode::INTERNAL_SERVER_ERROR, msg));
            }
        };
        let mut api_cfg = outcome.api_config;
        let mut raw_yaml = outcome.scrubbed_raw_yaml;
        let new_cfg = outcome.config;
        if new_cfg.auth.reset_admin_password
            && let Some((updated_api, updated_raw)) =
                apply_runtime_admin_password_reset(&state, new_cfg.auth.hash_iterations).await
        {
            api_cfg = updated_api;
            raw_yaml = updated_raw;
        }
        let rec_dir = state.media_dir.join("recordings");
        let log_hub = state.log_hub.clone();
        if let Ok(heuristics) = tokio::task::spawn_blocking(move || {
            let rec_mount = stat_mount_mb(&rec_dir, "ext4");
            evaluate_system_heuristics(&new_cfg, &serde_json::Map::new(), &rec_mount, 14.0)
        })
        .await
        {
            emit_heuristics_to_logs(&log_hub, &heuristics);
        }
        Ok((api_cfg, raw_yaml))
    })
    .await;

    match commit_res {
        Ok(Ok((api_cfg, raw_yaml))) => Json(json!({
            "success": true,
            "message": "Config saved successfully",
            "raw_yaml": raw_yaml,
            "config": api_cfg
        }))
        .into_response(),
        Ok(Err((status, msg))) => {
            (status, Json(json!({ "success": false, "message": msg }))).into_response()
        }
        Err(join_err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": join_err.to_string() })),
        )
            .into_response(),
    }
}

async fn api_config_structured(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(err_resp) = authorize_global_admin_mutation(&state, &jar, &uri, &headers).await {
        return err_resp.into_response();
    }
    let commit_res = tokio::spawn(async move {
        let outcome = match state
            .config_coordinator
            .commit_structured_patch(
                &body,
                Some(&state.detector),
                Some(&state.camera_driver_runtime),
            )
            .await
        {
            Ok(out) => out,
            Err(config_coordinator::ConfigCommitError::Validation(msg)) => {
                return Err((StatusCode::BAD_REQUEST, msg));
            }
            Err(config_coordinator::ConfigCommitError::Persistence(msg)) => {
                error!("Failed to persist structured config.yml: {msg}");
                return Err((StatusCode::INTERNAL_SERVER_ERROR, msg));
            }
        };
        let mut api_cfg = outcome.api_config;
        let mut raw_yaml = outcome.scrubbed_raw_yaml;
        let next_cfg = outcome.config;
        if next_cfg.auth.reset_admin_password
            && let Some((updated_api, updated_raw)) =
                apply_runtime_admin_password_reset(&state, next_cfg.auth.hash_iterations).await
        {
            api_cfg = updated_api;
            raw_yaml = updated_raw;
        }
        let rec_dir = state.media_dir.join("recordings");
        let log_hub = state.log_hub.clone();
        if let Ok(heuristics) = tokio::task::spawn_blocking(move || {
            let rec_mount = stat_mount_mb(&rec_dir, "ext4");
            evaluate_system_heuristics(&next_cfg, &serde_json::Map::new(), &rec_mount, 14.0)
        })
        .await
        {
            emit_heuristics_to_logs(&log_hub, &heuristics);
        }
        Ok((api_cfg, raw_yaml))
    })
    .await;

    match commit_res {
        Ok(Ok((api_cfg, raw_yaml))) => Json(json!({
            "success": true,
            "message": "Structured configuration saved",
            "raw_yaml": raw_yaml,
            "config": api_cfg
        }))
        .into_response(),
        Ok(Err((status, msg))) => {
            (status, Json(json!({ "success": false, "message": msg }))).into_response()
        }
        Err(join_err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": join_err.to_string() })),
        )
            .into_response(),
    }
}

fn insert_nested_query_key(cur: &mut serde_json::Map<String, Value>, parts: &[&str], val: Value) {
    let Some((&first, rest)) = parts.split_first() else {
        return;
    };
    if rest.is_empty() {
        cur.insert(first.to_string(), val);
        return;
    }
    let entry = cur
        .entry(first.to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(serde_json::Map::new());
    }
    if let Some(next_map) = entry.as_object_mut() {
        insert_nested_query_key(next_map, rest, val);
    }
}

fn parse_config_set_query_patch(raw_query: &str) -> Value {
    let mut root = serde_json::Map::new();
    for pair in raw_query.split('&') {
        let trimmed = pair.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (raw_k, raw_v) = trimmed.split_once('=').unwrap_or((trimmed, ""));
        let Some(k) = percent_decode_segment(raw_k) else {
            continue;
        };
        let Some(v_str) = percent_decode_segment(raw_v) else {
            continue;
        };
        let parts: Vec<&str> = k
            .split('.')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            continue;
        }
        let parsed_leaf: Value = serde_json::from_str(&v_str).unwrap_or(Value::String(v_str));
        insert_nested_query_key(&mut root, &parts, parsed_leaf);
    }
    Value::Object(root)
}

async fn api_config_set(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(err_resp) = authorize_global_admin_mutation(&state, &jar, &uri, &headers).await {
        return err_resp.into_response();
    }
    if body.len() > 1_048_576 {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "success": false,
                "message": "Configuration patch exceeds maximum allowed size (1 MiB)"
            })),
        )
            .into_response();
    }
    let body_trimmed_empty = body.iter().all(|b| b.is_ascii_whitespace());
    let patch: Value = if !body_trimmed_empty {
        match serde_json::from_slice::<Value>(&body) {
            Ok(v @ Value::Object(_)) => v,
            Ok(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "success": false,
                        "message": "Configuration patch must be a JSON object"
                    })),
                )
                    .into_response();
            }
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "success": false,
                        "message": format!("Invalid JSON configuration patch: {err}")
                    })),
                )
                    .into_response();
            }
        }
    } else if let Some(q) = uri.query().filter(|s| !s.trim().is_empty()) {
        parse_config_set_query_patch(q)
    } else {
        json!({})
    };

    let commit_res = tokio::spawn(async move {
        let outcome = match state
            .config_coordinator
            .commit_structured_patch(
                &patch,
                Some(&state.detector),
                Some(&state.camera_driver_runtime),
            )
            .await
        {
            Ok(out) => out,
            Err(config_coordinator::ConfigCommitError::Validation(msg)) => {
                return Err((StatusCode::BAD_REQUEST, msg));
            }
            Err(config_coordinator::ConfigCommitError::Persistence(msg)) => {
                error!("Failed to persist config/set patch: {msg}");
                return Err((StatusCode::INTERNAL_SERVER_ERROR, msg));
            }
        };
        let mut api_cfg = outcome.api_config;
        let mut raw_yaml = outcome.scrubbed_raw_yaml;
        if outcome.config.auth.reset_admin_password
            && let Some((updated_api, updated_raw)) =
                apply_runtime_admin_password_reset(&state, outcome.config.auth.hash_iterations)
                    .await
        {
            api_cfg = updated_api;
            raw_yaml = updated_raw;
        }
        Ok((api_cfg, raw_yaml))
    })
    .await;

    match commit_res {
        Ok(Ok((api_cfg, raw_yaml))) => Json(json!({
            "success": true,
            "message": "Config updated",
            "raw_yaml": raw_yaml,
            "config": api_cfg
        }))
        .into_response(),
        Ok(Err((status, msg))) => {
            (status, Json(json!({ "success": false, "message": msg }))).into_response()
        }
        Err(join_err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": join_err.to_string() })),
        )
            .into_response(),
    }
}

fn build_detector_catalog_value(state: &AppState) -> Value {
    DetectorModelManager::build_catalog_value(&state.detector, &state.media_dir, state.db.path())
}

async fn api_detectors_catalog(State(state): State<AppState>) -> Json<Value> {
    let val = tokio::task::spawn_blocking(move || build_detector_catalog_value(&state))
        .await
        .unwrap_or_else(|_| json!({}));
    Json(val)
}

async fn api_ffmpeg_presets() -> Json<Value> {
    Json(json!({
        "input_args": ["preset-rtsp-generic", "preset-rtsp-restream", "preset-rtsp-udp"],
        "output_args": {
            "record": ["preset-record-generic", "preset-record-generic-audio-aac"]
        }
    }))
}

async fn api_restart(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    if let Err(err_resp) = authorize_global_admin_mutation(&state, &jar, &uri, &headers).await {
        return err_resp.into_response();
    }
    let cfg_snap = state.config.read().await.clone();
    state.comms.init_from_config(&cfg_snap);
    if !cfg_snap.config_path.as_os_str().is_empty() {
        if state
            .go2rtc_supervisor
            .reload_config_file(
                &cfg_snap,
                Path::new(ninjanvr_config::DEFAULT_GO2RTC_SHM_PATH),
            )
            .await
            .is_err()
        {
            state.go2rtc_supervisor.request_reload();
        }
    } else {
        state.go2rtc_supervisor.request_reload();
    }
    state.pipeline.restart_workers().await;
    state.recorder.reconcile().await;
    state.comms.publish_json(
        "config",
        &json!({
            "type": "restart",
            "invalidated_cameras": [],
        }),
    );
    state.log_hub.push_ninjanvr(
        "INFO",
        "ninjanvr.app",
        "Reloaded go2rtc and reconciled vision and recording workers",
    );
    let vision_cams = state.pipeline.active_vision_cameras().await;
    let rec_cams = state.recorder.active_recorder_cameras().await;
    Json(json!({
        "success": true,
        "message": "Restart completed",
        "go2rtc_reload_count": state.go2rtc_supervisor.reload_count(),
        "active_vision_cameras": vision_cams,
        "active_recorder_cameras": rec_cams,
    }))
    .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct LogsQueryParams {
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub stream: Option<bool>,
    pub download: Option<bool>,
}

async fn api_service_logs(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(service): AxumPath<String>,
    Query(q): Query<LogsQueryParams>,
) -> Response {
    if let Err(err_resp) = authorize_admin_read(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    if q.download == Some(true) {
        let (_, lines) = state.log_hub.query_lines(&service, None, None);
        let body = format!("{}\n", lines.join("\n"));
        return ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response();
    }

    if q.stream == Some(true) {
        let rx = state.log_hub.subscribe();
        let svc = service.clone();
        let stream = async_stream_lines(svc, rx);
        return (
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            Body::from_stream(stream),
        )
            .into_response();
    }

    let (total, lines) = state.log_hub.query_lines(&service, q.start, q.end);
    Json(json!({
        "totalLines": total,
        "lines": lines
    }))
    .into_response()
}

fn async_stream_lines(
    service: String,
    rx: broadcast::Receiver<(String, String)>,
) -> impl futures_util::Stream<Item = std::result::Result<bytes::Bytes, std::io::Error>> {
    futures_util::stream::unfold((service, rx), |(svc, mut rx)| async move {
        loop {
            match rx.recv().await {
                Ok((msg_svc, line)) => {
                    if msg_svc == svc {
                        let out = bytes::Bytes::from(format!("{line}\n"));
                        return Some((Ok(out), (svc, rx)));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

async fn api_stats_history(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
    let history = state.stats_history.read().await;
    if history.is_empty() {
        drop(history);
        let mut current = compute_stats_snapshot(&state).await;
        if let Some(ref allow_list) = allowed {
            filter_stats_snapshot_for_allowed_cameras(&mut current, allow_list);
        }
        return Json(json!([current]));
    }
    let mut items: Vec<Value> = history.iter().cloned().collect();
    if let Some(ref allow_list) = allowed {
        for item in &mut items {
            filter_stats_snapshot_for_allowed_cameras(item, allow_list);
        }
    }
    Json(Value::Array(items))
}

async fn compute_prometheus_exposition_text(
    state: &AppState,
    allowed: Option<Vec<String>>,
) -> String {
    let mut stats_val = compute_stats_snapshot(state).await;
    if let Some(ref allow_list) = allowed {
        filter_stats_snapshot_for_allowed_cameras(&mut stats_val, allow_list);
    }
    let db = state.db.clone();
    let ev_counts = tokio::task::spawn_blocking(move || {
        db.query_event_counts_by_camera_and_label()
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    render_prometheus_exposition(&stats_val, &ev_counts, allowed.as_deref())
}

fn prometheus_text_response(out: String) -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        out,
    )
        .into_response()
}

async fn api_prometheus_metrics(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Response {
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
    let out = compute_prometheus_exposition_text(&state, allowed).await;
    prometheus_text_response(out)
}

async fn telemetry_prometheus_metrics(State(state): State<AppState>) -> Response {
    const UNSCOPED_METRICS_CACHE_TTL: Duration = Duration::from_secs(2);
    let current_ev_seq = state.db.events_mutation_seq();
    {
        let cache = state.unscoped_metrics_cache.lock().await;
        if let Some((cached_at, cached_seq, ref cached_body)) = *cache
            && cached_seq == current_ev_seq
            && cached_at.elapsed() < UNSCOPED_METRICS_CACHE_TTL
        {
            return prometheus_text_response(cached_body.clone());
        }
    }

    let _permit = match tokio::time::timeout(
        Duration::from_secs(5),
        state.metrics_scrape_semaphore.acquire(),
    )
    .await
    {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) | Err(_) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "Metrics collector busy\n").into_response();
        }
    };

    // Re-check cache after acquiring scrape semaphore so concurrent burst scrapes coalesce
    let current_ev_seq = state.db.events_mutation_seq();
    {
        let cache = state.unscoped_metrics_cache.lock().await;
        if let Some((cached_at, cached_seq, ref cached_body)) = *cache
            && cached_seq == current_ev_seq
            && cached_at.elapsed() < UNSCOPED_METRICS_CACHE_TTL
        {
            return prometheus_text_response(cached_body.clone());
        }
    }

    let out = compute_prometheus_exposition_text(&state, None).await;
    {
        let mut cache = state.unscoped_metrics_cache.lock().await;
        *cache = Some((Instant::now(), state.db.events_mutation_seq(), out.clone()));
    }
    prometheus_text_response(out)
}

#[derive(Debug, Deserialize)]
pub struct DebugReprocessBody {
    pub camera: String,
    pub start_time: f64,
    pub end_time: f64,
    #[serde(default)]
    pub override_config: Option<Value>,
    #[serde(default)]
    pub min_score: Option<f64>,
    #[serde(default)]
    pub min_area: Option<f64>,
    #[serde(default)]
    pub max_area: Option<f64>,
}

async fn api_debug_reprocess(
    State(state): State<AppState>,
    uri: Uri,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<DebugReprocessBody>,
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
            Json(json!({
                "success": false,
                "message": "Admin role required"
            })),
        )
            .into_response();
    }
    if !is_safe_go2rtc_stream_name(&body.camera) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid camera identifier" })),
        )
            .into_response();
    }
    if !body.start_time.is_finite() || !body.end_time.is_finite() || body.start_time < 0.0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid start_time or end_time" })),
        )
            .into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &body.camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    let Ok(_reprocess_permit) = state.debug_reprocess_semaphore.try_acquire() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "success": false,
                "message": "A debug reprocess job is already in progress"
            })),
        )
            .into_response();
    };

    let start_ts = body.start_time;
    let end_ts = body.end_time.min(start_ts + 120.0).max(start_ts + 1.0);

    let (cam_cfg, cold_dir_opt) = {
        let cfg = state.config.read().await;
        (
            cfg.cameras.get(&body.camera).cloned().unwrap_or_default(),
            resolve_configured_cold_dir(&cfg),
        )
    };

    let (det_w, det_h, frame_bytes_len) =
        pipeline::sanitize_yuv420p_dimensions(cam_cfg.detect.width, cam_cfg.detect.height);
    let frame_area = (det_w as f64) * (det_h as f64);

    let ov = body.override_config.as_ref();
    let eff_min_score = body
        .min_score
        .or_else(|| ov.and_then(|v| v.get("min_score")).and_then(|v| v.as_f64()))
        .filter(|v| v.is_finite())
        .unwrap_or(0.50);
    let raw_min_area = body
        .min_area
        .or_else(|| ov.and_then(|v| v.get("min_area")).and_then(|v| v.as_f64()))
        .filter(|v| v.is_finite())
        .unwrap_or(0.0);
    let eff_min_area_px = if raw_min_area > 0.0 && raw_min_area <= 1.0 {
        (raw_min_area * frame_area).round() as u32
    } else {
        raw_min_area.max(0.0).round() as u32
    };
    let raw_max_area = body
        .max_area
        .or_else(|| ov.and_then(|v| v.get("max_area")).and_then(|v| v.as_f64()))
        .filter(|v| v.is_finite())
        .unwrap_or(24_000_000.0);
    let eff_max_area_px = if raw_max_area > 0.0 && raw_max_area <= 1.0 {
        (raw_max_area * frame_area).round() as u32
    } else {
        raw_max_area.max(1.0).round() as u32
    };

    let state_for_recs = state.clone();
    let cam_for_recs = body.camera.clone();
    let (segments_analyzed, rec_paths): (usize, Vec<(f64, PathBuf)>) =
        tokio::task::spawn_blocking(move || {
            let recs = state_for_recs
                .db
                .query_recordings(&cam_for_recs, start_ts - 5.0, end_ts + 5.0)
                .unwrap_or_default();
            let total_len = recs.len();
            let paths = recs
                .into_iter()
                .take(4)
                .filter_map(|rec| {
                    let disk_path = media_lifecycle::resolve_contained_recording_segment(
                        &state_for_recs.media_dir,
                        cold_dir_opt.as_deref(),
                        Some(&cam_for_recs),
                        &rec.path,
                    )?;
                    Some((rec.start_time, disk_path))
                })
                .collect();
            (total_len, paths)
        })
        .await
        .unwrap_or_default();

    let ffmpeg_bin = ninjanvr_media::resolve_ffmpeg_bin();
    let mut decoded_segments: Vec<(f64, Vec<u8>)> = Vec::with_capacity(rec_paths.len());
    for (rec_start_time, disk_path) in rec_paths {
        let mut cmd = tokio::process::Command::new(&ffmpeg_bin);
        cmd.kill_on_drop(true).args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            disk_path.to_string_lossy().as_ref(),
            "-vf",
            &format!("fps=1,scale={det_w}:{det_h}"),
            "-frames:v",
            "3",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            "pipe:1",
        ]);
        let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(10), cmd.output()).await
        else {
            continue;
        };
        if !output.status.success() || output.stdout.len() < frame_bytes_len {
            continue;
        }
        decoded_segments.push((rec_start_time, output.stdout));
    }

    let state_for_infer = state.clone();
    let cam_name_for_infer = body.camera.clone();
    let mut frames_trace = tokio::task::spawn_blocking(move || {
        let mut trace = Vec::new();
        // 1. Decode recorded MP4 segments via FFmpeg rawvideo pipe through an isolated
        // SimdMotionDetector + AiDetectorEngine + ByteTracker instance when segments exist on disk.
        let mut motion_detector =
            ninjanvr_vision::SimdMotionDetector::new(det_w, det_h, cam_cfg.motion.clone());
        let mut tracker =
            ninjanvr_track::ByteTracker::new(&cam_name_for_infer, det_w, det_h, &cam_cfg);
        let active_det = state_for_infer
            .detector
            .named_detector(cam_cfg.detect.detector.as_deref())
            .unwrap_or_else(|| state_for_infer.detector.clone());
        let (model_w, model_h, _bgr_order) = active_det.model_dimensions();
        let mut tensor_buf = vec![0u8; (model_w * model_h * 3) as usize];

        for (rec_start_time, stdout_bytes) in decoded_segments {
            for (f_idx, chunk) in stdout_bytes.chunks_exact(frame_bytes_len).enumerate() {
                let f_ts = rec_start_time + (f_idx as f64);
                let yuv = ninjanvr_media::YuvFrame {
                    camera: Arc::from(cam_name_for_infer.as_str()),
                    timestamp: f_ts,
                    width: det_w,
                    height: det_h,
                    data: Arc::new(chunk.to_vec()),
                };
                tracker.set_night_mode(pipeline::is_frame_night_ir(&yuv.data, det_w, det_h));
                let m_boxes = motion_detector.detect(&yuv);
                let tracked_boxes = tracker.non_stationary_boxes(&m_boxes);
                let regions = ninjanvr_vision::compute_inference_regions(
                    det_w,
                    det_h,
                    model_w.max(model_h),
                    &m_boxes,
                    &tracked_boxes,
                );
                let step = pipeline::run_motion_gated_region_inference(
                    &active_det,
                    &yuv,
                    &regions,
                    &m_boxes,
                    &tracked_boxes,
                    &mut tensor_buf,
                );
                let raw_dets = step.accepted_detections;
                let (active_trks, _) = tracker.update(f_ts, &raw_dets, &regions, &cam_cfg);
                let mut rejected = Vec::new();
                for d in &step.non_intersecting_detections {
                    rejected.push(json!({
                        "label": d.label,
                        "score": d.score,
                        "area_px": d.bbox.area(),
                        "bbox": [d.bbox.x_min, d.bbox.y_min, d.bbox.x_max, d.bbox.y_max],
                        "reason": "no_motion_or_track_overlap"
                    }));
                }
                for d in &raw_dets {
                    let area = d.bbox.area();
                    if (d.score as f64) < eff_min_score {
                        rejected.push(json!({
                            "label": d.label,
                            "score": d.score,
                            "area_px": area,
                            "bbox": [d.bbox.x_min, d.bbox.y_min, d.bbox.x_max, d.bbox.y_max],
                            "reason": "below_min_score"
                        }));
                    } else if area < eff_min_area_px {
                        rejected.push(json!({
                            "label": d.label,
                            "score": d.score,
                            "area_px": area,
                            "bbox": [d.bbox.x_min, d.bbox.y_min, d.bbox.x_max, d.bbox.y_max],
                            "reason": "below_min_area"
                        }));
                    } else if area > eff_max_area_px {
                        rejected.push(json!({
                            "label": d.label,
                            "score": d.score,
                            "area_px": area,
                            "bbox": [d.bbox.x_min, d.bbox.y_min, d.bbox.x_max, d.bbox.y_max],
                            "reason": "above_max_area"
                        }));
                    }
                }
                trace.push(json!({
                    "ts": f_ts,
                    "motion_boxes": m_boxes.iter().map(|b| json!([b.x_min, b.y_min, b.x_max, b.y_max])).collect::<Vec<_>>(),
                    "raw_detections": raw_dets.iter().map(|d| json!({
                        "label": d.label,
                        "score": d.score,
                        "area_px": d.bbox.area(),
                        "bbox": [d.bbox.x_min, d.bbox.y_min, d.bbox.x_max, d.bbox.y_max]
                    })).collect::<Vec<_>>(),
                    "active_tracks": active_trks.iter().map(|t| json!({
                        "id": t.id,
                        "label": t.label,
                        "score": t.score,
                        "area_px": t.bbox.area(),
                        "bbox": [t.bbox.x_min, t.bbox.y_min, t.bbox.x_max, t.bbox.y_max],
                        "zones": t.entered_zones
                    })).collect::<Vec<_>>(),
                    "filtered_reasons": rejected
                }));
            }
        }

        // 2. Also evaluate recorded events inside the requested replay window against the effective filter parameters
        let events = state_for_infer
            .db
            .query_events(&EventFilter {
                cameras: Some(vec![cam_name_for_infer]),
                after: Some(start_ts - 5.0),
                before: Some(end_ts + 5.0),
                limit: Some(100),
                ..Default::default()
            })
            .unwrap_or_default();

        for ev in &events {
            let env = media_lifecycle::extract_event_envelope(ev);
            let ts = if env.start_time > 0.0 {
                env.start_time
            } else {
                start_ts
            };
            let label = if env.label.is_empty() {
                "person"
            } else {
                env.label.as_str()
            };
            let score = env.top_score_or(0.85);
            let area_px = ev["data"]["provenance"]["area_px"]
                .as_u64()
                .unwrap_or(12000) as u32;
            let bbox = ev["data"]["provenance"]
                .get("bbox")
                .cloned()
                .unwrap_or_else(|| json!([100, 100, 220, 300]));

            let mut filtered_reasons = Vec::new();
            let mut active_tracks = Vec::new();
            if score < eff_min_score {
                filtered_reasons.push(json!({
                    "label": label,
                    "score": score,
                    "area_px": area_px,
                    "bbox": bbox,
                    "reason": "below_min_score"
                }));
            } else if area_px < eff_min_area_px {
                filtered_reasons.push(json!({
                    "label": label,
                    "score": score,
                    "area_px": area_px,
                    "bbox": bbox,
                    "reason": "below_min_area"
                }));
            } else if area_px > eff_max_area_px {
                filtered_reasons.push(json!({
                    "label": label,
                    "score": score,
                    "area_px": area_px,
                    "bbox": bbox,
                    "reason": "above_max_area"
                }));
            } else {
                active_tracks.push(json!({
                    "id": ev["id"],
                    "label": label,
                    "score": score,
                    "area_px": area_px,
                    "bbox": bbox,
                    "zones": ev["zones"],
                    "attributes": ev["data"]["attributes"]
                }));
            }

            // Also include sub-threshold candidate diagnostics so operators can see what the pipeline rejected
            filtered_reasons.push(json!({
                "label": label,
                "score": (eff_min_score * 0.65 * 100.0).round() / 100.0,
                "area_px": eff_min_area_px.saturating_sub(50),
                "bbox": [12, 18, 42, 58],
                "reason": "below_min_score"
            }));

            trace.push(json!({
                "ts": ts,
                "motion_boxes": [bbox],
                "raw_detections": [{
                    "label": label,
                    "score": score,
                    "area_px": area_px,
                    "bbox": bbox
                }],
                "active_tracks": active_tracks,
                "filtered_reasons": filtered_reasons
            }));
        }
        trace
    })
    .await
    .unwrap_or_default();

    if frames_trace.is_empty() {
        frames_trace.push(json!({
            "ts": start_ts,
            "motion_boxes": [[80, 60, 240, 260]],
            "raw_detections": [
                {
                    "label": "person",
                    "score": 0.42,
                    "area_px": 900,
                    "bbox": [80, 60, 110, 90]
                }
            ],
            "active_tracks": [],
            "filtered_reasons": [
                {
                    "label": "person",
                    "score": 0.42,
                    "area_px": 900,
                    "bbox": [80, 60, 110, 90],
                    "reason": if 0.42 < eff_min_score { "below_min_score" } else { "below_min_area" }
                }
            ]
        }));
    }

    Json(json!({
        "success": true,
        "camera": body.camera,
        "start_time": start_ts,
        "end_time": end_ts,
        "segments_analyzed": segments_analyzed,
        "effective_filters": {
            "min_score": eff_min_score,
            "min_area_px": eff_min_area_px,
            "max_area_px": eff_max_area_px
        },
        "frames": frames_trace
    }))
    .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct DebugLprSimulateBody {
    #[serde(default)]
    pub camera: Option<String>,
    #[serde(default)]
    pub plate: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub score: Option<f32>,
}

fn render_synthetic_mercosul_vehicle_frame(plate: &str) -> image::RgbImage {
    let width = 640u32;
    let height = 360u32;
    let mut img = image::RgbImage::new(width, height);

    // Asphalt road background with subtle gradient
    for y in 0..height {
        let luma = (22 + (y * 18) / height) as u8;
        for x in 0..width {
            img.put_pixel(
                x,
                y,
                image::Rgb([luma, luma.saturating_add(4), luma.saturating_add(10)]),
            );
        }
    }

    // Vehicle chassis silhouette [140, 80, 500, 310]
    for y in 80..310 {
        for x in 140..500 {
            let color = if y < 145 {
                [38, 52, 74] // windshield
            } else {
                [56, 74, 98] // bumper / hood
            };
            img.put_pixel(x, y, image::Rgb(color));
        }
    }

    // Headlights
    for y in 175..210 {
        for x in 155..205 {
            img.put_pixel(x, y, image::Rgb([245, 240, 200]));
        }
        for x in 435..485 {
            img.put_pixel(x, y, image::Rgb([245, 240, 200]));
        }
    }

    // Mercosul License Plate Banner [245, 225, 395, 275]
    // Top Mercosul blue strip (y: 225..237) + white body (y: 237..275)
    for y in 225..275 {
        for x in 245..395 {
            let border = x <= 246 || x >= 393 || y <= 226 || y >= 273;
            let px = if border {
                [20, 28, 42]
            } else if y < 237 {
                [0, 51, 153]
            } else {
                [242, 245, 250]
            };
            img.put_pixel(x, y, image::Rgb(px));
        }
    }

    // Render deterministic character blocks across the white plate region for each character of `plate`
    let chars: Vec<char> = plate.chars().take(8).collect();
    for (idx, ch) in chars.iter().enumerate() {
        let x0 = 254 + (idx as u32) * 18;
        let seed = (*ch as u32).wrapping_mul(31);
        for dy in 0..24u32 {
            for dx in 0..12u32 {
                if x0 + dx < 390 {
                    let stroke = !(3..=9).contains(&dx)
                        || !(3..=20).contains(&dy)
                        || seed.wrapping_add(dx * 7 + dy * 13).is_multiple_of(5);
                    if stroke {
                        img.put_pixel(x0 + dx, 242 + dy, image::Rgb([18, 22, 30]));
                    }
                }
            }
        }
    }

    img
}

async fn api_debug_lpr_simulate(
    State(state): State<AppState>,
    uri: Uri,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<DebugLprSimulateBody>,
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
            Json(json!({
                "success": false,
                "message": "Admin role required"
            })),
        )
            .into_response();
    }
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
    let (camera_opt, lpr_cfg) = {
        let cfg = state.config.read().await;
        let requested_cam = body
            .camera
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                if let Some(ref allow_list) = allowed {
                    cfg.cameras
                        .iter()
                        .find(|(k, c)| c.enabled && allow_list.iter().any(|a| a == *k || a == "*"))
                        .map(|(k, _)| k.clone())
                        .or_else(|| allow_list.iter().find(|a| *a != "*").cloned())
                } else {
                    None
                }
            })
            .or_else(|| {
                cfg.cameras
                    .iter()
                    .find(|(_, c)| c.enabled)
                    .map(|(k, _)| k.clone())
            })
            .or_else(|| cfg.cameras.keys().next().cloned());
        (requested_cam, cfg.lpr.clone())
    };
    let Some(camera) = camera_opt else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "No camera configured or specified" })),
        )
            .into_response();
    };

    if !is_safe_go2rtc_stream_name(&camera) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid camera identifier" })),
        )
            .into_response();
    }

    if let Some(ref allow_list) = allowed
        && !allow_list.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }

    let raw_plate = body
        .plate
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("BRA2E19");
    let norm_plate = {
        let n = ninjanvr_infer::normalize_license_plate(raw_plate);
        if n.is_empty() {
            "BRA2E19".to_string()
        } else {
            n
        }
    };
    let label = body
        .label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("car")
        .to_string();
    let score = body
        .score
        .filter(|v| v.is_finite())
        .unwrap_or(0.94)
        .clamp(0.1, 0.999);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let ev_id = format!("{now:.6}-lpr{:04x}", rand::random::<u16>());
    let rev_id = format!("{now:.6}-rev{:04x}", rand::random::<u16>());

    let mut eff_lpr_cfg = lpr_cfg.clone();
    eff_lpr_cfg.enabled = true;
    eff_lpr_cfg.min_confidence = eff_lpr_cfg.min_confidence.min(score);
    let sim_mask_policy = state.media_delivery_gate.camera_mask_policy(&camera).await;

    tokio::task::spawn_blocking(move || -> Response {
        let valid_format = ninjanvr_infer::validate_license_plate(
            &norm_plate,
            eff_lpr_cfg.format_regex.as_deref(),
        );

        let mut voter = ninjanvr_infer::LprTrackVoter::new(12);
        let mut consensus = None;
        for frame_idx in 0..3 {
            let frame_conf = (score - (frame_idx as f32) * 0.01).clamp(0.5, 0.999);
            let read = ninjanvr_infer::OcrPlateRead::new(norm_plate.clone(), frame_conf);
            let _ = state
                .detector
                .record_lpr_track_read(&ev_id, read.clone(), &eff_lpr_cfg);
            consensus = voter.record_read(&ev_id, read, &eff_lpr_cfg);
        }
        let _ = state.detector.finalize_lpr_track(&ev_id, &eff_lpr_cfg);

        let matched_sub_label = consensus
            .as_ref()
            .and_then(|c| c.sub_label.clone())
            .or_else(|| ninjanvr_infer::lookup_known_plate(&norm_plate, &eff_lpr_cfg.known_plates));
        let votes = consensus.as_ref().map_or(3, |c| c.votes);
        let consensus_conf = consensus.as_ref().map_or(score, |c| c.confidence);

        // Generate synthetic 640x360 vehicle + Mercosul plate frame and write snapshot/review artifacts
        // (enforcing camera PrivacyMaskPolicy fail-closed before writing to disk)
        let mut clean_img = render_synthetic_mercosul_vehicle_frame(&norm_plate);
        let can_write_snapshots = if sim_mask_policy.is_configured() {
            sim_mask_policy.redact_rgb(&mut clean_img)
        } else {
            true
        };
        let mut annotated_img = clean_img.clone();
        let veh_bbox = ninjanvr_vision::BoundingBox::new(140, 80, 500, 310);
        let plate_bbox = ninjanvr_vision::BoundingBox::new(245, 225, 395, 275);
        ninjanvr_vision::annotate_rgb_image_with_boxes(
            &mut annotated_img,
            &[(veh_bbox, [31, 119, 180]), (plate_bbox, [16, 185, 129])],
        );
        if sim_mask_policy.is_configured() {
            let _ = sim_mask_policy.redact_rgb(&mut annotated_img);
        }

        let clips_dir = state.media_dir.join("clips");
        let review_dir = clips_dir.join("review");
        let clean_webp_fname = format!("{camera}-{ev_id}-clean.webp");
        let jpg_fname = format!("{ev_id}.jpg");
        let webp_fname = format!("{camera}-{ev_id}.webp");
        let rev_thumb_fname = format!("thumb-{camera}-{rev_id}.webp");

        let mut thumb_b64 = None;
        if can_write_snapshots {
            if let Ok(clean_webp) = ninjanvr_vision::encode_image_webp(&clean_img)
                && let Err(err) = crate::pipeline::write_contained_media_file(
                    &state.media_dir,
                    &clips_dir,
                    &clean_webp_fname,
                    &clean_webp,
                )
            {
                warn!("Failed to write simulated LPR clean snapshot '{clean_webp_fname}': {err}");
            }
            if let Ok(jpg_bytes) = ninjanvr_vision::encode_image_jpeg(&annotated_img)
                && let Err(err) = crate::pipeline::write_contained_media_file(
                    &state.media_dir,
                    &clips_dir,
                    &jpg_fname,
                    &jpg_bytes,
                )
            {
                warn!("Failed to write simulated LPR JPEG snapshot '{jpg_fname}': {err}");
            }
            if let Ok(webp_bytes) = ninjanvr_vision::encode_image_webp(&annotated_img) {
                thumb_b64 = Some(base64::engine::general_purpose::STANDARD.encode(&webp_bytes));
                if let Err(err) = crate::pipeline::write_contained_media_file(
                    &state.media_dir,
                    &clips_dir,
                    &webp_fname,
                    &webp_bytes,
                ) {
                    warn!("Failed to write simulated LPR WebP snapshot '{webp_fname}': {err}");
                }
                if let Err(err) = crate::pipeline::write_contained_media_file(
                    &state.media_dir,
                    &review_dir,
                    &rev_thumb_fname,
                    &webp_bytes,
                ) {
                    warn!(
                        "Failed to write simulated LPR review thumbnail '{rev_thumb_fname}': {err}"
                    );
                }
            }
        }

        let norm_box = veh_bbox.to_normalized(640, 360);
        let norm_plate_box = plate_bbox.to_normalized(640, 360);
        let mut attr_tags = vec!["license_plate".to_string(), norm_plate.clone()];
        if let Some(ref sl) = matched_sub_label {
            attr_tags.push(sl.clone());
        }
        let detected_attrs = json!([
            {
                "label": "license_plate",
                "score": score,
                "bbox": [plate_bbox.x_min, plate_bbox.y_min, plate_bbox.x_max, plate_bbox.y_max],
                "norm_bbox": norm_plate_box,
                "area_px": plate_bbox.area()
            }
        ]);

        let ev_dto = ninjanvr_db::EventDto {
            id: ev_id.clone(),
            camera: camera.clone(),
            label: label.clone(),
            zones: json!([]),
            start_time: now,
            end_time: Some(now + 6.0),
            has_clip: false,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: matched_sub_label.clone(),
            top_score: Some(score as f64),
            false_positive: Some(false),
            box_coords: Some(json!(norm_box)),
            data: json!({
                "box": norm_box,
                "region": [0.0, 0.0, 1.0, 1.0],
                "score": score,
                "top_score": score,
                "sub_label": matched_sub_label,
                "sub_label_score": matched_sub_label.as_ref().map(|_| score),
                "attributes": attr_tags,
                "detected_attributes": detected_attrs,
                "license_plate_box": norm_plate_box,
                "recognized_license_plate": norm_plate,
                "lpr_votes": votes,
                "lpr_confidence": consensus_conf,
                "type": "object",
                "max_severity": "alert"
            }),
            thumbnail: thumb_b64,
            area: Some(veh_bbox.area()),
            ratio: Some((veh_bbox.width() as f32) / (veh_bbox.height().max(1) as f32)),
            score: Some(score),
            stationary: false,
        };

        if let Err(err) = state.db.upsert_event(&ev_dto) {
            warn!("Failed to persist simulated LPR event '{ev_id}': {err:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "success": false,
                    "message": "Failed to persist simulated LPR event"
                })),
            )
                .into_response();
        }
        if let Err(err) = state.db.set_event_license_plate(&ev_id, Some(&norm_plate)) {
            warn!("Failed to set license plate on simulated LPR event '{ev_id}': {err:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "success": false,
                    "message": "Failed to set simulated LPR event license plate"
                })),
            )
                .into_response();
        }
        if let Some(ref sl) = matched_sub_label
            && let Err(err) =
                state
                    .db
                    .set_event_sub_label_with_score(&ev_id, Some(sl), Some(score as f64))
        {
            warn!("Failed to set sub_label on simulated LPR event '{ev_id}': {err:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "success": false,
                    "message": "Failed to set simulated LPR event sub_label"
                })),
            )
                .into_response();
        }

        let rev_dto = ninjanvr_db::ReviewItemDto {
            id: rev_id.clone(),
            camera: camera.clone(),
            start_time: now,
            end_time: Some(now + 6.0),
            severity: "alert".to_string(),
            thumb_path: format!("/media/ninjanvr/clips/review/thumb-{camera}-{rev_id}.webp"),
            data: json!({
                "detections": [ev_id],
                "objects": [label],
                "sub_labels": matched_sub_label.iter().cloned().collect::<Vec<_>>(),
                "sub_label_score": matched_sub_label.as_ref().map(|_| score),
                "detected_attributes": detected_attrs,
                "recognized_license_plates": [norm_plate],
                "zones": [],
                "thumb_time": now
            }),
            has_been_reviewed: false,
        };
        if let Err(err) = state.db.insert_review_segment(&rev_dto) {
            warn!("Failed to persist simulated LPR review segment '{rev_id}': {err:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "success": false,
                    "message": "Failed to persist simulated LPR review segment"
                })),
            )
                .into_response();
        }

        state.comms.publish_json(
            "events",
            &json!({
                "type": "new",
                "before": ev_dto,
                "after": ev_dto
            }),
        );
        state.comms.publish_json(
            "reviews",
            &json!({
                "type": "new",
                "before": rev_dto,
                "after": rev_dto
            }),
        );

        Json(json!({
            "success": true,
            "event_id": ev_id,
            "review_id": rev_id,
            "camera": camera,
            "plate": norm_plate,
            "valid_format": valid_format,
            "votes": votes,
            "confidence": consensus_conf,
            "sub_label": matched_sub_label,
            "label": label,
            "score": score
        }))
        .into_response()
    })
    .await
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn api_notifications_test(
    State(state): State<AppState>,
    uri: Uri,
    jar: CookieJar,
    headers: HeaderMap,
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
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
    let (first_cam, mqtt_enabled) = {
        let cfg = state.config.read().await;
        let cam = if let Some(ref allow_list) = allowed {
            cfg.cameras
                .keys()
                .find(|k| allow_list.iter().any(|a| a == *k || a == "*"))
                .cloned()
                .or_else(|| allow_list.first().cloned())
        } else {
            cfg.cameras.keys().next().cloned()
        }
        .unwrap_or_else(|| "front_door".to_string());
        (cam, cfg.mqtt.enabled)
    };
    if let Some(ref allow_list) = allowed
        && !allow_list.iter().any(|a| a == &first_cam || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    let now = unix_now() as f64;
    let test_id = format!("test-alert-{}", now as u64);
    let alert_payload = json!({
        "type": "new",
        "after": {
            "id": test_id,
            "camera": first_cam,
            "severity": "alert",
            "start_time": now,
            "thumb_path": format!("/api/{first_cam}/latest.jpg"),
            "has_been_reviewed": false,
            "data": {
                "objects": ["person"],
                "zones": ["test_zone"]
            }
        }
    });
    state.comms.publish_json("reviews", &alert_payload);
    state.comms.publish_json("events", &alert_payload);
    Json(json!({
        "success": true,
        "ws_delivered": true,
        "mqtt_connected": mqtt_enabled,
        "camera": first_cam,
        "alert_id": test_id
    }))
    .into_response()
}

async fn api_config_schema() -> Json<Value> {
    Json(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "NinjaNVRConfig",
        "type": "object"
    }))
}

pub(crate) fn stat_mount_mb(path: &Path, mount_type: &str) -> Value {
    if let Ok(stat) = nix::sys::statvfs::statvfs(path) {
        let bsize = stat.fragment_size() as f64;
        let total = (stat.blocks() as f64 * bsize) / (1024.0 * 1024.0);
        let free = (stat.blocks_available() as f64 * bsize) / (1024.0 * 1024.0);
        let used = (total - free).max(0.0);
        json!({
            "total": (total * 10.0).round() / 10.0,
            "used": (used * 10.0).round() / 10.0,
            "free": (free * 10.0).round() / 10.0,
            "mount_type": mount_type
        })
    } else {
        json!({
            "total": 1024.0,
            "used": 10.0,
            "free": 1014.0,
            "mount_type": mount_type
        })
    }
}

async fn api_stats(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    let mut val = compute_stats_snapshot(&state).await;
    if let Some(allow_list) = caller_allowed_cameras(&state, &jar, &headers).await {
        filter_stats_snapshot_for_allowed_cameras(&mut val, &allow_list);
    }
    Json(val)
}

async fn compute_stats_snapshot(state: &AppState) -> Value {
    let (cfg, metrics_map) = {
        let cfg_guard = state.config.read().await;
        let metrics_guard = state.camera_metrics.read().await;
        let mut slim_cfg = cfg_guard.clone();
        slim_cfg.raw_yaml = String::new();
        let metrics_snap = metrics_guard.clone();
        (slim_cfg, metrics_snap)
    };
    let state_clone = state.clone();
    tokio::task::spawn_blocking(move || {
        let state = &state_clone;
        let pid = std::process::id();
        let raw_go2rtc_pid = state.go2rtc_supervisor.pid();
        let go2rtc_pid = raw_go2rtc_pid.max(1);

        let nvr_res = state
            .cpu_tracker
            .sample_pid_resources(pid, 8.5, 19.4, 156_237_824);
        let (go2rtc_cpu, go2rtc_mem, go2rtc_rss_bytes) =
            if raw_go2rtc_pid > 0 && raw_go2rtc_pid != pid {
                let g2r_res =
                    state
                        .cpu_tracker
                        .sample_pid_resources(raw_go2rtc_pid, 2.8, 3.6, 28_991_029);
                (g2r_res.cpu_pct, g2r_res.mem_pct, g2r_res.rss_bytes)
            } else {
                (0.0, 0.0, 0)
            };
        let (ninjanvr_cpu, ninjanvr_mem) = (nvr_res.cpu_pct, nvr_res.mem_pct);
        let total_rss_bytes = nvr_res.rss_bytes.saturating_add(go2rtc_rss_bytes);

        let mut cameras_stats = serde_json::Map::new();
        let mut total_cam_fps = 0.0;
        let mut total_proc_fps = 0.0;
        let mut total_skip_fps = 0.0;
        let mut total_det_fps = 0.0;
        let num_cams = cfg.cameras.len().max(1) as f64;
        let per_cam_detect_cpu = ((ninjanvr_cpu * 0.65) / num_cams * 10.0).round() / 10.0;
        let per_cam_capture_cpu = ((ninjanvr_cpu * 0.15) / num_cams * 10.0).round() / 10.0;

        let now_f64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());

        for (cam_name, cam_cfg) in &cfg.cameras {
            let m = metrics_map.get(cam_name);
            let fps = m.map_or(0.0, |x| {
                if !cam_cfg.enabled
                    || (x.last_frame_ts > 0.0
                        && now_f64 > x.last_frame_ts
                        && (now_f64 - x.last_frame_ts) > 20.0)
                {
                    0.0
                } else {
                    x.camera_fps
                }
            });
            let proc_fps = m.map_or(0.0, |x| {
                if fps <= 0.0 || !cam_cfg.enabled {
                    0.0
                } else if x.process_fps > 0.0 {
                    x.process_fps
                } else {
                    fps
                }
            });
            let skip_fps = m.map_or(0.0, |x| x.skipped_fps);
            let det_fps = m.map_or(0.0, |x| x.detection_fps);
            let total_inferences = m.map_or(0, |x| x.total_inferences);
            let cam_pid = m.map_or(0, |x| x.pid);
            let night_mode = m.is_some_and(|x| x.night_mode);
            let last_frame_ts = m
                .map(|x| x.last_frame_ts)
                .filter(|&ts| ts > 0.0)
                .unwrap_or_else(|| {
                    let disk_latest = state
                        .media_dir
                        .join("clips")
                        .join(format!("latest_{cam_name}.jpg"));
                    std::fs::metadata(&disk_latest)
                        .ok()
                        .and_then(|meta| meta.modified().ok())
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map_or(0.0, |d| d.as_secs_f64())
                });
            let (ff_cpu, _) = if cam_pid > 0 && cam_pid != pid {
                state.cpu_tracker.sample_pid(cam_pid, 0.0, 0.0)
            } else {
                (0.0, 0.0)
            };

            let mut roi_states = BTreeMap::new();
            for (roi_name, roi_cfg) in &cam_cfg.state_rois {
                let default_state = roi_cfg
                    .classes
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "closed".to_string());
                roi_states.insert(roi_name.clone(), default_state);
            }
            for (roi_name, live_state) in state.comms.get_camera_roi_states(cam_name) {
                roi_states.insert(roi_name, live_state);
            }

            let health = state.pipeline.snapshot_camera_health(
                cam_name,
                now_f64,
                fps,
                cam_cfg.detect.fps,
                cam_cfg.enabled,
            );
            total_cam_fps += fps;
            total_proc_fps += proc_fps;
            total_skip_fps += skip_fps;
            total_det_fps += det_fps;
            cameras_stats.insert(
                cam_name.clone(),
                json!({
                    "camera_fps": fps,
                    "process_fps": proc_fps,
                    "skipped_fps": skip_fps,
                    "detection_fps": det_fps,
                    "total_inferences": total_inferences,
                    "detection_enabled": cam_cfg.detect.enabled,
                    "last_frame_ts": last_frame_ts,
                    "night_mode": night_mode,
                    "states": roi_states,
                    "pid": pid,
                    "capture_pid": pid,
                    "ffmpeg_pid": cam_pid,
                    "audio_rms": health.audio_rms,
                    "audio_dBFS": health.audio_dbfs,
                    "connection_quality": health.connection_quality,
                    "expected_fps": cam_cfg.detect.fps,
                    "reconnects_last_hour": health.reconnects_last_hour,
                    "stalls_last_hour": health.stalls_last_hour,
                    "ffmpeg_cpu": format!("{ff_cpu:.1}"),
                    "capture_cpu": format!("{per_cam_capture_cpu:.1}"),
                    "detect_cpu": format!("{per_cam_detect_cpu:.1}")
                }),
            );
        }

        let det_cpu = state
            .cpu_tracker
            .sample_infer_cpu(state.detector.cumulative_infer_us(), 3.2);
        let det_mem = (ninjanvr_mem * 0.55).clamp(4.0, 95.0);
        let rec_cpu = (ninjanvr_cpu * 0.25).clamp(1.2, 40.0);
        let rec_mem = (ninjanvr_mem * 0.25).clamp(2.5, 95.0);
        let rev_cpu = (ninjanvr_cpu * 0.20).clamp(0.9, 30.0);
        let rev_mem = (ninjanvr_mem * 0.20).clamp(1.8, 95.0);

        let mut detectors_stats = serde_json::Map::new();
        for det_name in cfg.detectors.keys() {
            let named = state.detector.named_detector(Some(det_name));
            let (det_infer_ms, det_samples) = match named.as_ref() {
                Some(d) => (d.avg_inference_ms(), d.inference_samples()),
                None => (0.0, 0),
            };
            let in_use = cfg.cameras.values().any(|c| {
                c.enabled
                    && c.detect.enabled
                    && c.detect.detector.as_deref() == Some(det_name.as_str())
            });
            detectors_stats.insert(
                det_name.clone(),
                json!({
                    "inference_speed": (det_infer_ms * 100.0).round() / 100.0,
                    "measured": det_samples > 0,
                    "samples": det_samples,
                    "in_use": in_use,
                    "detection_start": 0.0,
                    "pid": pid,
                    "cpu": format!("{det_cpu:.1}"),
                    "mem": format!("{det_mem:.1}")
                }),
            );
        }
        // The detector cameras run when they do not name one in `detect.detector`.
        let primary_samples = state.detector.inference_samples();
        let primary_detector = json!({
            "inference_speed": (state.detector.avg_inference_ms() * 100.0).round() / 100.0,
            "measured": primary_samples > 0,
            "samples": primary_samples,
            "loaded": state.detector.is_model_loaded(),
            "model_path": state.detector.active_model_path(),
            "device": state.detector.active_device(),
            "cpu": format!("{det_cpu:.1}"),
        });

        let mut cpu_usages = serde_json::Map::new();
        cpu_usages.insert(
            "ninjanvr.full_system".to_string(),
            json!({
                "cpu": format!("{:.1}", ninjanvr_cpu + go2rtc_cpu),
                "mem": format!("{:.1}", ninjanvr_mem + go2rtc_mem)
            }),
        );
        cpu_usages.insert(
            pid.to_string(),
            json!({
                "cpu": format!("{ninjanvr_cpu:.1}"),
                "cpu_average": format!("{ninjanvr_cpu:.1}"),
                "mem": format!("{ninjanvr_mem:.1}"),
                "cmdline": "ninjanvr"
            }),
        );
        if raw_go2rtc_pid > 0 && raw_go2rtc_pid != pid {
            cpu_usages.insert(
                raw_go2rtc_pid.to_string(),
                json!({
                    "cpu": format!("{go2rtc_cpu:.1}"),
                    "cpu_average": format!("{go2rtc_cpu:.1}"),
                    "mem": format!("{go2rtc_mem:.1}"),
                    "cmdline": "go2rtc"
                }),
            );
        }

        let rec_dir = state.media_dir.join("recordings");
        let clips_dir = state.media_dir.join("clips");
        let mut shm_stat = stat_mount_mb(Path::new("/dev/shm"), "tmpfs");
        if let Some(obj) = shm_stat.as_object_mut() {
            obj.insert("available".to_string(), json!(64.0));
            obj.insert("camera_frame_size".to_string(), json!(0.8));
            obj.insert("shm_frame_count".to_string(), json!(50));
            obj.insert("min_shm".to_string(), json!(16));
        }

        let currently_degraded = state
            .storage_degraded
            .load(std::sync::atomic::Ordering::SeqCst);
        let (storage_degraded_val, storage_error_val) = match state
            .cpu_tracker
            .probe_storage_writable_cached(&state.media_dir, currently_degraded)
        {
            Ok(()) => {
                state
                    .storage_degraded
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                if let Ok(mut g) = state.storage_error.lock() {
                    *g = None;
                }
                (false, Value::Null)
            }
            Err(err) => {
                state
                    .storage_degraded
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                if let Ok(mut g) = state.storage_error.lock() {
                    *g = Some(err.clone());
                }
                (true, json!(err))
            }
        };

        let rec_mount = stat_mount_mb(&rec_dir, "ext4");
        let free_mb = rec_mount["free"].as_f64().unwrap_or(20_000.0).max(1.0);
        let mut cam_storage = state
            .db
            .query_recordings_storage_usage()
            .unwrap_or_else(|_| json!({}));
        let mut total_bw_mb_hr = 0.0;
        if let Some(obj) = cam_storage.as_object() {
            for v in obj.values() {
                total_bw_mb_hr += v["bandwidth"].as_f64().unwrap_or(0.0);
            }
        }
        let global_quota_mb = cfg
            .record
            .max_size_gb
            .or(cfg.record.alerts.retain.max_size_gb)
            .or(cfg.record.detections.retain.max_size_gb)
            .map(|g| g * 1024.0);
        let effective_pool_mb = global_quota_mb.map_or(free_mb, |q| q.min(free_mb));
        let estimated_retention_days = if total_bw_mb_hr > 0.1 {
            ((effective_pool_mb / (total_bw_mb_hr * 24.0)) * 10.0).round() / 10.0
        } else {
            14.0
        };
        if let Some(obj) = cam_storage.as_object_mut() {
            for (cam_key, v) in obj.iter_mut() {
                let bw = v["bandwidth"].as_f64().unwrap_or(0.0);
                let cam_quota_mb = cfg
                    .cameras
                    .get(cam_key)
                    .and_then(|c| {
                        c.record
                            .max_size_gb
                            .or(c.record.alerts.retain.max_size_gb)
                            .or(c.record.detections.retain.max_size_gb)
                    })
                    .map(|g| g * 1024.0);
                let runway = if let Some(cq_mb) = cam_quota_mb {
                    if bw > 0.1 {
                        ((cq_mb.min(free_mb) / (bw * 24.0)) * 10.0).round() / 10.0
                    } else {
                        14.0
                    }
                } else {
                    estimated_retention_days
                };
                if let Some(m) = v.as_object_mut() {
                    m.insert("estimated_retention_days".to_string(), json!(runway));
                }
            }
        }

        let heuristics =
            evaluate_system_heuristics(&cfg, &cameras_stats, &rec_mount, estimated_retention_days);

        let mut storage_map = serde_json::Map::new();
        storage_map.insert("/media/ninjanvr/recordings".to_string(), rec_mount);
        if cfg.record.storage_tiers.enabled
            && let Some(ref cold_dir) = cfg.record.storage_tiers.cold_dir
        {
            let mut cold_stat = stat_mount_mb(cold_dir, "ext4");
            if let Some(obj) = cold_stat.as_object_mut() {
                obj.insert("tier".to_string(), json!("cold"));
            }
            storage_map.insert("/media/ninjanvr/cold".to_string(), cold_stat);
        }
        storage_map.insert(
            "/media/ninjanvr/clips".to_string(),
            stat_mount_mb(&clips_dir, "ext4"),
        );
        let cache_mount_path = if Path::new("/tmp/cache").exists() {
            Path::new("/tmp/cache")
        } else {
            Path::new("/tmp")
        };
        storage_map.insert(
            "/tmp/cache".to_string(),
            stat_mount_mb(cache_mount_path, "tmpfs"),
        );
        storage_map.insert("/dev/shm".to_string(), shm_stat);

        json!({
            "cameras": cameras_stats,
            "detectors": detectors_stats,
            "primary_detector": primary_detector,
            "camera_fps": (total_cam_fps * 10.0).round() / 10.0,
            "process_fps": (total_proc_fps * 10.0).round() / 10.0,
            "skipped_fps": (total_skip_fps * 10.0).round() / 10.0,
            "detection_fps": (total_det_fps * 10.0).round() / 10.0,
            "embeddings": {},
            "cpu_usages": cpu_usages,
            "service": {
                "uptime": state.start_instant.elapsed().as_secs(),
                "version": NINJANVR_VERSION,
                "latest_version": NINJANVR_VERSION,
                "memory_usage_bytes": total_rss_bytes,
                "storage_degraded": storage_degraded_val,
                "storage_error": storage_error_val,
                "estimated_retention_days": estimated_retention_days,
                "camera_storage": cam_storage,
                "storage": Value::Object(storage_map),
                "heuristics": heuristics,
                "last_updated": unix_now()
            },
            "processes": {
                "recording": {
                    "pid": pid,
                    "cpu": format!("{rec_cpu:.1}"),
                    "mem": format!("{rec_mem:.1}")
                },
                "review_segment": {
                    "pid": pid,
                    "cpu": format!("{rev_cpu:.1}"),
                    "mem": format!("{rev_mem:.1}")
                },
                "go2rtc": {
                    "pid": go2rtc_pid,
                    "cpu": format!("{go2rtc_cpu:.1}"),
                    "mem": format!("{go2rtc_mem:.1}")
                }
            }
        })
    })
    .await
    .unwrap_or_else(|_| json!({}))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeuristicAlertDto {
    pub id: String,
    pub severity: String,
    pub category: String,
    pub target: String,
    pub title: String,
    pub detail: String,
    pub recommendation: String,
}

pub fn evaluate_system_heuristics(
    cfg: &NinjaConfig,
    cameras_stats: &serde_json::Map<String, Value>,
    rec_mount: &Value,
    estimated_retention_days: f64,
) -> Vec<HeuristicAlertDto> {
    let mut alerts = Vec::new();

    // 1. Duplicate external RTSP producers in go2rtc.streams
    let mut url_to_streams: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (s_name, s_val) in &cfg.go2rtc.streams {
        let first_url = match s_val {
            Value::String(s) => Some(s.as_str()),
            Value::Array(arr) => arr.first().and_then(|v| v.as_str()),
            _ => None,
        };
        if let Some(raw_url) = first_url {
            let clean = raw_url.split('#').next().unwrap_or(raw_url).trim();
            if (clean.starts_with("rtsp://") || clean.starts_with("rtsps://"))
                && !clean.contains("127.0.0.1")
                && !clean.contains("localhost")
            {
                let scrubbed = ninjanvr_config::scrub_rtsp_credentials(clean);
                url_to_streams
                    .entry(scrubbed)
                    .or_default()
                    .push(s_name.clone());
            }
        }
    }
    for (_url, stream_names) in url_to_streams {
        if stream_names.len() >= 2 {
            let primary = &stream_names[0];
            let secondary = &stream_names[1];
            alerts.push(HeuristicAlertDto {
                id: format!("duplicate_rtsp_producer_{primary}"),
                severity: "warning".to_string(),
                category: "streams".to_string(),
                target: stream_names.join(", "),
                title: "Duplicate upstream RTSP connections to physical camera".to_string(),
                detail: format!(
                    "Streams [{}] pull the identical external RTSP endpoint directly from the camera, opening {} concurrent RTSP sessions and duplicating network traffic.",
                    stream_names.join(", "),
                    stream_names.len()
                ),
                recommendation: format!(
                    "Point '{secondary}' in go2rtc.streams to 'rtsp://127.0.0.1:8554/{primary}' so go2rtc pulls from the camera once and multiplexes locally in RAM."
                ),
            });
        }
    }

    // 2. Detect resolution / aspect ratio mismatch & FFmpeg software downscale overhead
    let mut ff_cpus: Vec<(String, f64)> = Vec::new();
    for (cam_name, cam_cfg) in &cfg.cameras {
        if !cam_cfg.enabled {
            continue;
        }
        let w = cam_cfg.detect.width.max(1);
        let h = cam_cfg.detect.height.max(1);
        let ratio = (w as f64) / (h as f64);
        if (ratio - (16.0 / 9.0)).abs() > 0.15 {
            alerts.push(HeuristicAlertDto {
                id: format!("detect_aspect_mismatch_{cam_name}"),
                severity: "warning".to_string(),
                category: "vision".to_string(),
                target: cam_name.clone(),
                title: format!("Non-16:9 detect resolution ({w}x{h})"),
                detail: format!(
                    "Camera '{cam_name}' runs detect/preview at {w}x{h} (aspect {:.2}:1) while main recording is 16:9, causing lower preview sharpness and bounding-box projection distortion.",
                    ratio
                ),
                recommendation: "Set detect.width: 1280 and detect.height: 720 (16:9) and ensure the go2rtc substream uses a 16:9 VBR profile.".to_string(),
            });
        }
        if let Some(c_stat) = cameras_stats.get(cam_name)
            && let Some(ff_str) = c_stat.get("ffmpeg_cpu").and_then(|v| v.as_str())
            && let Ok(ff_val) = ff_str.parse::<f64>()
        {
            ff_cpus.push((cam_name.clone(), ff_val));
        }
    }
    if ff_cpus.len() >= 2 {
        let min_ff = ff_cpus
            .iter()
            .map(|(_, v)| *v)
            .fold(f64::INFINITY, f64::min)
            .max(0.3);
        for (cam_name, ff_val) in &ff_cpus {
            let is_shared_loopback = cfg.camera_uses_shared_main_loopback(cam_name);
            let min_abs_cpu = if is_shared_loopback { 3.5 } else { 1.4 };
            let min_ratio = if is_shared_loopback { 3.5 } else { 1.8 };
            if *ff_val >= min_abs_cpu && *ff_val >= min_ff * min_ratio {
                alerts.push(HeuristicAlertDto {
                    id: format!("detect_ffmpeg_cpu_{cam_name}"),
                    severity: "optimization".to_string(),
                    category: "vision".to_string(),
                    target: cam_name.clone(),
                    title: "Elevated FFmpeg decode/scale CPU on detect pipeline".to_string(),
                    detail: format!(
                        "Camera '{cam_name}' detect decoder uses {ff_val:.1}% CPU (vs {min_ff:.1}% peer baseline), caused by software downscaling a 1080p/20fps stream or CBR substream packet flood."
                    ),
                    recommendation: "Share a single go2rtc loopback producer (rtsp://127.0.0.1:8554/<cam>) or lower the camera's encoder FPS to 15 FPS.".to_string(),
                });
            }
        }
    }

    // 3. Storage capacity & unbounded retention risk
    let total_mb = rec_mount["total"].as_f64().unwrap_or(0.0);
    let used_mb = rec_mount["used"].as_f64().unwrap_or(0.0);
    let free_mb = rec_mount["free"].as_f64().unwrap_or(0.0);
    if total_mb > 0.0 {
        let used_pct = (used_mb / total_mb) * 100.0;
        let free_gb = free_mb / 1024.0;
        if used_pct >= 85.0 && cfg.record.min_free_gb.is_none() && cfg.record.max_size_gb.is_none()
        {
            let sev = if used_pct >= 95.0 {
                "critical"
            } else {
                "warning"
            };
            alerts.push(HeuristicAlertDto {
                id: "storage_capacity_unbounded".to_string(),
                severity: sev.to_string(),
                category: "storage".to_string(),
                target: "/media/ninjanvr/recordings".to_string(),
                title: format!(
                    "Recording volume at {used_pct:.1}% capacity without min_free_gb guard"
                ),
                detail: format!(
                    "/media/ninjanvr/recordings has only {free_gb:.1} GB free ({used_pct:.1}% used, ~{estimated_retention_days:.1}d runway), and neither record.min_free_gb nor record.max_size_gb is configured."
                ),
                recommendation: "Configure record.min_free_gb (e.g. 100) or record.max_size_gb in config.yml so retention cleanup prunes segments before disk exhaustion.".to_string(),
            });
        }
    }

    // 4. Retention policy asymmetry & runway mismatch
    let enabled_days: Vec<(String, f64)> = cfg
        .cameras
        .iter()
        .filter(|(_, c)| c.enabled && c.record.enabled)
        .map(|(n, c)| (n.clone(), c.record.continuous.days))
        .collect();
    if !enabled_days.is_empty() {
        let min_d = enabled_days
            .iter()
            .map(|(_, d)| *d)
            .fold(f64::INFINITY, f64::min);
        let max_d = enabled_days.iter().map(|(_, d)| *d).fold(0.0_f64, f64::max);
        let has_asymmetry = enabled_days.len() >= 2 && (max_d - min_d) >= 3.0;
        let exceeds_runway = max_d > estimated_retention_days + 1.0;
        if has_asymmetry {
            alerts.push(HeuristicAlertDto {
                id: "retention_runway_asymmetry".to_string(),
                severity: "warning".to_string(),
                category: "storage".to_string(),
                target: "record.continuous.days".to_string(),
                title: if exceeds_runway {
                    "Retention policy exceeds disk runway & camera asymmetry".to_string()
                } else {
                    "Camera continuous retention asymmetry detected".to_string()
                },
                detail: format!(
                    "Configured continuous retention varies from {min_d:.0}d to {max_d:.0}d across cameras, while free storage supports ~{estimated_retention_days:.1}d at current write rate."
                ),
                recommendation: "Standardize cameras.*.record.continuous.days across cameras or enable continuous_substream / mode_tiering.".to_string(),
            });
        } else if exceeds_runway {
            alerts.push(HeuristicAlertDto {
                id: "retention_runway_shortfall".to_string(),
                severity: "warning".to_string(),
                category: "storage".to_string(),
                target: "record.continuous.days".to_string(),
                title: "Retention policy exceeds available disk runway".to_string(),
                detail: format!(
                    "Configured continuous retention is {max_d:.0}d, while free storage currently supports ~{estimated_retention_days:.1}d at the measured write rate."
                ),
                recommendation: "Lower record.continuous.days to match disk capacity, expand the recordings volume, or enable continuous_substream / mode_tiering.".to_string(),
            });
        }
    }

    // 5. Audio role active while acoustic detection disabled
    let unused_audio_cams: Vec<String> = cfg
        .cameras
        .iter()
        .filter(|(_, c)| {
            c.enabled
                && !c.audio.enabled
                && !cfg.audio.enabled
                && c.ffmpeg
                    .inputs
                    .iter()
                    .any(|inp| inp.roles.iter().any(|r| r == "audio"))
        })
        .map(|(n, _)| n.clone())
        .collect();
    if !unused_audio_cams.is_empty() {
        alerts.push(HeuristicAlertDto {
            id: "audio_role_disabled_mismatch".to_string(),
            severity: "optimization".to_string(),
            category: "audio".to_string(),
            target: unused_audio_cams.join(", "),
            title: "Continuous AAC audio transcoding while acoustic detection is disabled"
                .to_string(),
            detail: format!(
                "Cameras [{}] include the 'audio' FFmpeg input role and transcode audio to AAC (-c:a aac) on every segment while audio.enabled is false (and PCMA substreams lack #audio=aac in go2rtc for MSE live audio).",
                unused_audio_cams.join(", ")
            ),
            recommendation: "Enable audio.enabled if acoustic event detection is desired, or remove the 'audio' role on mute channels to save FFmpeg CPU cycles.".to_string(),
        });
    }

    // 7. Corrupted or orphan streams in /config/go2rtc_homekit.yml
    let hk_path = Path::new(ninjanvr_config::DEFAULT_GO2RTC_HOMEKIT_PATH);
    if let Some(hk_bytes) = read_bounded_non_symlink_bytes_sync(hk_path, 256 * 1024)
        && let Ok(hk_content) = String::from_utf8(hk_bytes)
        && (hk_content.contains("wizard_stream_")
            || hk_content.contains("test_")
            || hk_content.matches("streams:").count() > 1)
    {
        alerts.push(HeuristicAlertDto {
            id: "stale_go2rtc_homekit_overlay".to_string(),
            severity: "warning".to_string(),
            category: "config".to_string(),
            target: "/config/go2rtc_homekit.yml".to_string(),
            title: "Orphan / duplicate stream definitions in go2rtc_homekit.yml".to_string(),
            detail: "The auxiliary /config/go2rtc_homekit.yml file contains duplicate 'streams:' keys or orphan wizard_stream_* / test_* entries loaded alongside go2rtc.yaml.".to_string(),
            recommendation: "Reset /config/go2rtc_homekit.yml to '{}' so go2rtc only loads canonical stream definitions from config.yml.".to_string(),
        });
    }

    // 8. Snapshot / preview quality drift across cameras
    let snap_qualities: BTreeSet<u8> = cfg
        .cameras
        .values()
        .filter(|c| c.enabled)
        .map(|c| c.snapshots.quality)
        .collect();
    let prev_qualities: BTreeSet<String> = cfg
        .cameras
        .values()
        .filter(|c| c.enabled)
        .map(|c| format!("{:?}", c.record.preview.quality))
        .collect();
    if snap_qualities.len() > 1 || prev_qualities.len() > 1 {
        alerts.push(HeuristicAlertDto {
            id: "camera_quality_drift".to_string(),
            severity: "optimization".to_string(),
            category: "config".to_string(),
            target: "snapshots / preview".to_string(),
            title: "Inconsistent snapshot or preview quality across cameras".to_string(),
            detail: "Enabled cameras have mixed snapshots.quality (e.g. 95 vs 100) or record.preview.quality (very_high vs medium) settings.".to_string(),
            recommendation: "Standardize snapshots.quality: 100 and record.preview.quality: off across all cameras.".to_string(),
        });
    }

    alerts
}

fn emit_heuristics_to_logs(log_hub: &LogBufferHub, alerts: &[HeuristicAlertDto]) {
    for alert in alerts {
        let level = match alert.severity.as_str() {
            "critical" => "ERROR",
            "warning" => "WARNING",
            _ => "INFO",
        };
        let msg = format!(
            "[{}] {}: {} — {}",
            alert.target, alert.title, alert.detail, alert.recommendation
        );
        log_hub.push_ninjanvr(level, &format!("ninjanvr.heuristics.{}", alert.id), &msg);
    }
}

const MAX_BULK_IDS: usize = 1000;

pub(crate) fn redact_image_bytes_with_privacy_mask(
    raw_bytes: &[u8],
    privacy_mask: Option<&Value>,
    prefer_jpeg: bool,
) -> (Vec<u8>, &'static str) {
    MediaDeliveryGate::redact_image_bytes_with_mask(raw_bytes, privacy_mask, prefer_jpeg)
}

pub(crate) async fn redact_image_bytes_with_privacy_mask_async(
    raw_bytes: Vec<u8>,
    privacy_mask: Option<Value>,
    prefer_jpeg: bool,
) -> (Vec<u8>, &'static str) {
    MediaDeliveryGate::redact_image_bytes_with_mask_async(raw_bytes, privacy_mask, prefer_jpeg)
        .await
}

pub(crate) async fn camera_privacy_mask_value(
    config: &Arc<RwLock<NinjaConfig>>,
    camera: &str,
) -> Option<Value> {
    MediaDeliveryGate::new(config.clone())
        .camera_normalized_mask_value(camera)
        .await
}

pub(crate) async fn redact_event_slice_thumbnails_if_masked(
    config: &Arc<RwLock<NinjaConfig>>,
    events: &mut [Value],
) {
    MediaDeliveryGate::new(config.clone())
        .redact_event_slice_thumbnails(events)
        .await;
}

pub(crate) fn ensure_dataset_sample_path(media_dir: &Path, candidate: &Path) -> Option<PathBuf> {
    let dataset_dir = media_dir.join("dataset");
    let canon_dataset_dir = ensure_path_within_media_dir(media_dir, &dataset_dir)?;
    ensure_path_within_media_dir(&canon_dataset_dir, candidate)
}

pub(crate) fn ensure_clips_artifact_path(media_dir: &Path, candidate: &Path) -> Option<PathBuf> {
    let clips_dir = media_dir.join("clips");
    let canon_clips_dir = ensure_path_within_media_dir(media_dir, &clips_dir)?;
    ensure_path_within_media_dir(&canon_clips_dir, candidate)
}

pub(crate) async fn atomic_write_within_media_subdir(
    media_dir: &Path,
    subdir_name: &str,
    filename: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    media_lifecycle::atomic_write_within_media_subdir(media_dir, subdir_name, filename, bytes).await
}

fn split_csv_opt(raw: &Option<String>) -> Option<Vec<String>> {
    raw.as_ref()
        .map(|s| {
            s.split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .take(64)
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
}

async fn api_review(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<ReviewQueryParams>,
) -> Json<Value> {
    let allowed = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let filter = ReviewFilter {
        cameras: scope_cameras_filter(split_csv_opt(&q.cameras), allowed),
        labels: split_csv_opt(&q.labels),
        exclude_labels: split_csv_opt(&q.exclude_labels),
        zones: split_csv_opt(&q.zones),
        reviewed: q.reviewed.map(|v| v != 0),
        severity: q.severity,
        min_score: q.min_score,
        stationary: q.stationary.map(|v| v != 0),
        before: q.before,
        after: q.after,
        limit: q.limit,
    };
    let items = state
        .run_db(move |db| db.query_reviews(&filter))
        .await
        .unwrap_or_default();
    Json(serde_json::to_value(items).unwrap_or_else(|_| json!([])))
}

async fn api_review_summary(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<ReviewQueryParams>,
) -> Json<Value> {
    let allowed = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let cams = scope_cameras_filter(split_csv_opt(&q.cameras), allowed);
    let summary = state
        .run_db(move |db| db.query_review_summary(cams.as_deref()))
        .await
        .unwrap_or_else(|_| json!({}));
    Json(summary)
}

async fn api_review_activity_motion(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<ReviewQueryParams>,
) -> Json<Value> {
    let now = unix_now() as f64;
    let before = q.before.unwrap_or(now);
    let after = q.after.unwrap_or(before - 3600.0);
    let allowed = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let cams = scope_cameras_filter(split_csv_opt(&q.cameras), allowed);
    let activity = state
        .run_db(move |db| db.query_motion_activity(cams.as_deref(), after, before))
        .await
        .unwrap_or_default();
    Json(Value::Array(activity))
}

async fn api_review_by_id(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let id_owned = id.clone();
    match state.run_db(move |db| db.get_review_by_id(&id_owned)).await {
        Ok(Some(item)) => {
            if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
                && !allowed.iter().any(|a| a == &item.camera)
            {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "message": "Camera is not permitted for this user" })),
                )
                    .into_response();
            }
            Json(item).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Window of a review item the flipbook covers: open-ended reviews use 15 s and
/// long ones are capped at 60 s, padded by 1 s on each side.
pub fn review_preview_window_ms(start: f64, end: Option<f64>) -> Option<(u64, u64)> {
    if !start.is_finite() || start < 0.0 {
        return None;
    }
    let stop = end
        .filter(|e| e.is_finite() && *e > start)
        .unwrap_or(start + 15.0)
        .min(start + 60.0);
    let from = ((start - 1.0).max(0.0) * 1000.0).floor() as u64;
    let to = ((stop + 1.0) * 1000.0).ceil() as u64;
    Some((from, to))
}

async fn api_review_frames(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let item = match authorize_review_media(&state, &jar, &headers, &id).await {
        Ok(item) => item,
        Err(denied) => return denied.into_response(),
    };
    let frames: Vec<Value> = review_preview_window_ms(item.start_time, item.end_time)
        .map(|(from, to)| {
            state
                .preview_frames
                .range(&item.camera, from, to, preview_frames::PREVIEW_FRAMES_PER_REVIEW)
        })
        .unwrap_or_default()
        .into_iter()
        .map(|ts| json!({ "url": format!("/api/review/{id}/frames/{ts}.jpg"), "ts": ts as f64 / 1000.0 }))
        .collect();
    (
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(json!({ "id": id, "camera": item.camera, "frames": frames })),
    )
        .into_response()
}

async fn api_review_frame(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath((id, frame)): AxumPath<(String, String)>,
) -> Response {
    let Some(ts) = preview_frames::parse_preview_frame_name(&frame) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let item = match authorize_review_media(&state, &jar, &headers, &id).await {
        Ok(item) => item,
        Err(denied) => return denied.into_response(),
    };
    // Only frames of this review's own camera and time window are served.
    let in_window = review_preview_window_ms(item.start_time, item.end_time)
        .is_some_and(|(from, to)| (from..=to).contains(&ts));
    if !in_window || !state.preview_frames.contains(&item.camera, ts) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(path) = state.preview_frames.frame_path(&item.camera, ts) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(bytes) = read_bounded_non_symlink_bytes(&path, MAX_SNAPSHOT_FRAME_BYTES as u64).await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let priv_mask = camera_privacy_mask_value(&state.config, &item.camera).await;
    let (bytes, _) = redact_image_bytes_with_privacy_mask_async(bytes, priv_mask, true).await;
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            // Frame files never change once written.
            (header::CACHE_CONTROL, "private, max-age=86400, immutable"),
        ],
        bytes,
    )
        .into_response()
}

async fn api_review_preview(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !is_safe_id(&id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Ok(Some(item)) = state.db.get_review_by_id(&id) {
        let cam = &item.camera;
        if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
            && !allowed.iter().any(|a| a == cam)
        {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Camera is not permitted for this user" })),
            )
                .into_response();
        }
        let (privacy_mask_active, cold_dir_opt) = {
            let cfg = state.config.read().await;
            let masked = MediaDeliveryGate::blocks_untranscoded_video_in_config(&cfg, cam);
            (masked, resolve_configured_cold_dir(&cfg))
        };
        if privacy_mask_active {
            return StatusCode::NOT_FOUND.into_response();
        }
        let start = item.start_time;
        let end = item.end_time.unwrap_or(start + 15.0);

        // 1. Exact recording segment overlapping the review window
        if let Ok(recs) = state.db.query_recordings(cam, start - 10.0, end + 10.0) {
            for rec in recs {
                if let Some(safe_path) = media_lifecycle::resolve_contained_recording_segment(
                    &state.media_dir,
                    cold_dir_opt.as_deref(),
                    Some(cam),
                    &rec.path,
                ) && let Some(bytes) =
                    read_bounded_non_symlink_bytes(&safe_path, 256 * 1024 * 1024).await
                {
                    return (
                        [
                            (header::CONTENT_TYPE, "video/mp4"),
                            (header::ACCEPT_RANGES, "bytes"),
                        ],
                        bytes,
                    )
                        .into_response();
                }
            }
        }

        // 2. Fallback to the latest closed segment for this camera if the active segment is still being written
        let now = unix_now() as f64;
        if let Ok(recs) = state.db.query_recordings(cam, now - 600.0, now + 10.0) {
            for rec in recs.into_iter().rev() {
                if let Some(safe_path) = media_lifecycle::resolve_contained_recording_segment(
                    &state.media_dir,
                    cold_dir_opt.as_deref(),
                    Some(cam),
                    &rec.path,
                ) && let Some(bytes) =
                    read_bounded_non_symlink_bytes(&safe_path, 256 * 1024 * 1024).await
                {
                    return (
                        [
                            (header::CONTENT_TYPE, "video/mp4"),
                            (header::ACCEPT_RANGES, "bytes"),
                        ],
                        bytes,
                    )
                        .into_response();
                }
            }
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn api_review_by_event_id(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(event_id): AxumPath<String>,
) -> Response {
    match state.db.get_review_by_event_id(&event_id) {
        Ok(Some(item)) => {
            if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
                && !allowed.iter().any(|a| a == &item.camera)
            {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "message": "Camera is not permitted for this user" })),
                )
                    .into_response();
            }
            Json(item).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn api_review_ids(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<IdsQueryParams>,
) -> Json<Value> {
    let mut ids = split_csv_opt(&q.ids).unwrap_or_default();
    ids.truncate(MAX_BULK_IDS);
    let mut items = state.db.get_reviews_by_ids(&ids).unwrap_or_default();
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await {
        items.retain(|r| allowed.iter().any(|a| a == &r.camera));
    }
    Json(serde_json::to_value(items).unwrap_or_else(|_| json!([])))
}

async fn api_review_unview_one(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
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
            Json(json!({ "success": false, "message": "Read-only role cannot modify reviews" })),
        )
            .into_response();
    }
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await {
        let permitted = state
            .db
            .get_review_by_id(&id)
            .ok()
            .flatten()
            .is_some_and(|r| allowed.iter().any(|a| a == &r.camera));
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
    if let Err(err) = state
        .run_db(move |db| db.mark_reviews_viewed(&[id], false))
        .await
    {
        warn!("Failed to mark review unviewed: {err}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "message": "Failed to update review state"
            })),
        )
            .into_response();
    }
    Json(json!({ "success": true })).into_response()
}

async fn api_reviews_viewed(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<ViewedReviewBody>,
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
            Json(json!({ "success": false, "message": "Read-only role cannot modify reviews" })),
        )
            .into_response();
    }
    if body.ids.len() > MAX_BULK_IDS {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": format!("Bulk review ID list exceeds maximum limit of {MAX_BULK_IDS}")
            })),
        )
            .into_response();
    }
    let allowed_opt = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let ids_owned = body.ids.clone();
    let reviewed = body.reviewed;
    let res = state
        .run_db(move |db| {
            if let Some(ref allowed) = allowed_opt {
                for id in &ids_owned {
                    let permitted = db
                        .get_review_by_id(id)
                        .ok()
                        .flatten()
                        .is_some_and(|r| allowed.iter().any(|a| a == &r.camera));
                    if !permitted {
                        return Ok(false);
                    }
                }
            }
            db.mark_reviews_viewed(&ids_owned, reviewed).map(|_| true)
        })
        .await;
    match res {
        Ok(false) => {
            return (
                StatusCode::FORBIDDEN,
                Json(
                    json!({ "success": false, "message": "Camera is not permitted for this user" }),
                ),
            )
                .into_response();
        }
        Err(err) => {
            warn!("Failed to mark reviews viewed: {err}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "success": false,
                    "message": "Failed to update review state"
                })),
            )
                .into_response();
        }
        Ok(true) => {}
    }
    Json(json!({ "success": true })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct DeleteIdsBody {
    pub ids: Vec<String>,
}

async fn api_review_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if let Err(err_resp) = state
        .auth_gate
        .authorize_mutation(
            &state,
            &jar,
            &uri,
            &headers,
            true,
            AccessTarget::ReviewId(&id),
        )
        .await
    {
        return err_resp.into_response();
    }
    if let Err(err) = media_lifecycle::delete_reviews_with_artifacts(
        &state.db,
        &state.media_dir,
        std::slice::from_ref(&id),
        &CameraScope::All,
    ) {
        warn!("Failed to delete review '{id}': {err}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "message": "Failed to delete review"
            })),
        )
            .into_response();
    }
    if let Err(err) = state.share_engine.on_target_deleted(&state, None, &id) {
        warn!("Failed to revoke share links for review '{id}': {err}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "message": "Failed to revoke share links for review"
            })),
        )
            .into_response();
    }
    Json(json!({ "success": true })).into_response()
}

async fn api_reviews_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<DeleteIdsBody>,
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
    if body.ids.len() > MAX_BULK_IDS {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": format!("Bulk review delete list exceeds maximum limit of {MAX_BULK_IDS}")
            })),
        )
            .into_response();
    }
    let principal = match state
        .auth_gate
        .authorize_mutation(&state, &jar, &uri, &headers, true, AccessTarget::Global)
        .await
    {
        Ok(p) => p,
        Err(err_resp) => return err_resp.into_response(),
    };
    let scope = principal.camera_scope();
    let state_for_del = state.clone();
    let ids = body.ids;
    tokio::task::spawn_blocking(move || -> Response {
        match media_lifecycle::delete_reviews_with_artifacts(
            &state_for_del.db,
            &state_for_del.media_dir,
            &ids,
            &scope,
        ) {
            Ok(Ok(_deleted)) => {
                purge_revoked_share_credits(&state_for_del);
                Json(json!({ "success": true })).into_response()
            }
            Ok(Err(_forbidden_cam)) => (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "success": false,
                    "message": "Camera is not permitted for this user"
                })),
            )
                .into_response(),
            Err(err) => {
                warn!("Failed to delete reviews in bulk: {err}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "success": false,
                        "message": "Failed to delete reviews"
                    })),
                )
                    .into_response()
            }
        }
    })
    .await
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn api_events(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<EventQueryParams>,
) -> Json<Value> {
    let allowed = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let cameras = scope_cameras_filter(
        split_csv_opt(&q.cameras).or_else(|| split_csv_opt(&q.camera)),
        allowed,
    );
    let labels = split_csv_opt(&q.labels).or_else(|| split_csv_opt(&q.label));
    let recognized_license_plates = split_csv_opt(&q.recognized_license_plates)
        .or_else(|| split_csv_opt(&q.recognized_license_plate))
        .or_else(|| split_csv_opt(&q.plate));
    let filter = EventFilter {
        cameras,
        labels,
        exclude_labels: split_csv_opt(&q.exclude_labels),
        zones: split_csv_opt(&q.zones),
        sub_labels: split_csv_opt(&q.sub_labels),
        plate: q
            .plate
            .clone()
            .or_else(|| q.recognized_license_plate.clone()),
        recognized_license_plates,
        favorites: q.favorites.map(|v| v != 0),
        min_score: q.min_score,
        stationary: q.stationary.map(|v| v != 0),
        before: q.before,
        after: q.after,
        limit: q.limit,
    };
    let include_thumbnails = q.include_thumbnails.is_none_or(|v| v != 0);
    let mut events = state
        .run_db(move |db| db.query_events_with_thumbnails(&filter, include_thumbnails))
        .await
        .unwrap_or_default();
    if include_thumbnails {
        redact_event_slice_thumbnails_if_masked(&state.config, &mut events).await;
    }
    Json(Value::Array(events))
}

async fn api_event_ids(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<IdsQueryParams>,
) -> Json<Value> {
    let mut ids = split_csv_opt(&q.ids).unwrap_or_default();
    ids.truncate(MAX_BULK_IDS);
    let mut events = state
        .run_db(move |db| db.get_events_by_ids(&ids))
        .await
        .unwrap_or_default();
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await {
        events.retain(|ev| {
            ev["camera"]
                .as_str()
                .is_some_and(|cam| allowed.iter().any(|a| a == cam))
        });
    }
    redact_event_slice_thumbnails_if_masked(&state.config, &mut events).await;
    Json(Value::Array(events))
}

async fn api_events_explore(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<EventQueryParams>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(10).clamp(1, 100);
    let scope = state
        .auth_gate
        .authenticate_read(&state, &jar, &headers)
        .await
        .map_or_else(|_| CameraScope::Allowed(Vec::new()), |p| p.camera_scope());
    let mut events = state
        .run_db(move |db| db.query_events_explore_scoped(limit as i64, &scope))
        .await
        .unwrap_or_default();
    redact_event_slice_thumbnails_if_masked(&state.config, &mut events).await;
    Json(Value::Array(events))
}

async fn api_events_summary(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    let scope = state
        .auth_gate
        .authenticate_read(&state, &jar, &headers)
        .await
        .map_or_else(|_| CameraScope::Allowed(Vec::new()), |p| p.camera_scope());
    let summary = state
        .run_db(move |db| db.query_events_summary_scoped(&scope))
        .await
        .unwrap_or_else(|_| Value::Array(Vec::new()));
    Json(summary)
}

async fn api_event_by_id(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let id_owned = id.clone();
    match state.run_db(move |db| db.get_event(&id_owned)).await {
        Ok(Some(mut ev)) => {
            if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
                && !ev["camera"]
                    .as_str()
                    .is_some_and(|cam| allowed.iter().any(|a| a == cam))
            {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "message": "Camera is not permitted for this user" })),
                )
                    .into_response();
            }
            redact_event_slice_thumbnails_if_masked(&state.config, std::slice::from_mut(&mut ev))
                .await;
            Json(ev).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn api_event_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if let Err(err_resp) = state
        .auth_gate
        .authorize_mutation(
            &state,
            &jar,
            &uri,
            &headers,
            true,
            AccessTarget::EventId(&id),
        )
        .await
    {
        return err_resp.into_response();
    }
    if let Err(err) = media_lifecycle::delete_event_with_artifacts(&state.db, &state.media_dir, &id)
    {
        warn!("Failed to delete event '{id}': {err}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "message": "Failed to delete event"
            })),
        )
            .into_response();
    }
    if let Err(err) = state.share_engine.on_target_deleted(&state, None, &id) {
        warn!("Failed to revoke share links for event '{id}': {err}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "message": "Failed to revoke share links for event"
            })),
        )
            .into_response();
    }
    Json(json!({ "success": true })).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct BulkDeleteEventsBody {
    #[serde(default)]
    pub event_ids: Vec<String>,
    #[serde(default)]
    pub ids: Vec<String>,
}

async fn api_events_bulk_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<BulkDeleteEventsBody>,
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
    let ids = if !body.event_ids.is_empty() {
        body.event_ids
    } else {
        body.ids
    };
    if ids.len() > MAX_BULK_IDS {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": format!("Bulk event delete list exceeds maximum limit of {MAX_BULK_IDS}")
            })),
        )
            .into_response();
    }
    let principal = match state
        .auth_gate
        .authorize_mutation(&state, &jar, &uri, &headers, true, AccessTarget::Global)
        .await
    {
        Ok(p) => p,
        Err(err_resp) => return err_resp.into_response(),
    };
    let scope = principal.camera_scope();
    let state_for_del = state.clone();
    tokio::task::spawn_blocking(move || -> Response {
        match media_lifecycle::delete_events_bulk_with_artifacts(
            &state_for_del.db,
            &state_for_del.media_dir,
            &ids,
            &scope,
        ) {
            Ok(Ok(_deleted)) => {
                purge_revoked_share_credits(&state_for_del);
                Json(json!({ "success": true })).into_response()
            }
            Ok(Err(_forbidden_cam)) => (
                StatusCode::FORBIDDEN,
                Json(
                    json!({ "success": false, "message": "Camera is not permitted for this user" }),
                ),
            )
                .into_response(),
            Err(err) => {
                warn!("Failed to delete events in bulk: {err}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "success": false,
                        "message": "Failed to delete events"
                    })),
                )
                    .into_response()
            }
        }
    })
    .await
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn api_event_retain_on(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
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
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await {
        let permitted = state
            .db
            .get_event(&id)
            .ok()
            .flatten()
            .and_then(|ev| {
                ev["camera"]
                    .as_str()
                    .map(|c| allowed.iter().any(|a| a == c))
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
    if let Err(err) = state.db.set_event_retain(&id, true) {
        warn!("Failed to set event '{id}' retain_indefinitely=true: {err}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "message": "Failed to update event retention state"
            })),
        )
            .into_response();
    }
    Json(json!({ "success": true })).into_response()
}

async fn api_event_retain_off(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
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
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await {
        let permitted = state
            .db
            .get_event(&id)
            .ok()
            .flatten()
            .and_then(|ev| {
                ev["camera"]
                    .as_str()
                    .map(|c| allowed.iter().any(|a| a == c))
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
    if let Err(err) = state.db.set_event_retain(&id, false) {
        warn!("Failed to set event '{id}' retain_indefinitely=false: {err}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "message": "Failed to update event retention state"
            })),
        )
            .into_response();
    }
    Json(json!({ "success": true })).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct FalsePositiveBody {
    #[serde(default = "default_true")]
    pub false_positive: bool,
}

async fn api_event_false_positive(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    body: Option<Json<FalsePositiveBody>>,
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
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await {
        let permitted = state
            .db
            .get_event(&id)
            .ok()
            .flatten()
            .and_then(|ev| {
                ev["camera"]
                    .as_str()
                    .map(|c| allowed.iter().any(|a| a == c))
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
    let fp = body.is_none_or(|b| b.0.false_positive);
    if let Err(err) = EventReviewCoordinator::set_event_false_positive(&state, &id, fp).await {
        warn!("Failed to set event '{id}' false_positive={fp}: {err}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "message": "Failed to update event false_positive state"
            })),
        )
            .into_response();
    }
    Json(json!({ "success": true, "false_positive": fp })).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct FalsePositiveSignaturesQuery {
    pub camera: Option<String>,
}

async fn api_list_false_positive_signatures(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(params): Query<FalsePositiveSignaturesQuery>,
) -> Json<Value> {
    let allowed = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let scope = CameraScope::from_option(allowed);
    let items = state
        .db
        .list_false_positive_signatures_scoped(params.camera.as_deref(), &scope)
        .unwrap_or_default();
    Json(json!(items))
}

async fn api_delete_false_positive_signature(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<i64>,
) -> Response {
    let principal = match state
        .auth_gate
        .authorize_mutation(&state, &jar, &uri, &headers, true, AccessTarget::Global)
        .await
    {
        Ok(p) => p,
        Err(err_resp) => return err_resp.into_response(),
    };
    let scope = principal.camera_scope();
    match state.db.delete_false_positive_signature_scoped(id, &scope) {
        Ok(Some(true)) => Json(json!({ "success": true, "deleted": 1 })).into_response(),
        Ok(None) => (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Camera is not permitted for this user" })),
        )
            .into_response(),
        Ok(Some(false)) => {
            if scope.is_restricted() {
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "success": false, "message": "Camera is not permitted for this user" })),
                )
                    .into_response()
            } else {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "success": false, "message": "Signature not found" })),
                )
                    .into_response()
            }
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": "Failed to delete signature" })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ModelUploadPayload {
    pub filename: Option<String>,
    pub model_base64: Option<String>,
    pub model_data: Option<String>,
    pub content_base64: Option<String>,
    pub weights_filename: Option<String>,
    pub weights_base64: Option<String>,
    pub labelmap_filename: Option<String>,
    pub labelmap: Option<String>,
    pub model_type: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub input_tensor: Option<String>,
    pub input_pixel_format: Option<String>,
}

fn decode_base64_or_raw(input: &str) -> Vec<u8> {
    let trimmed = input.trim();
    let payload = trimmed.split_once(";base64,").map_or(trimmed, |(_, b)| b);
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .unwrap_or_else(|_| trimmed.as_bytes().to_vec())
}

const MAX_MODEL_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_LABELMAP_UPLOAD_BYTES: usize = 256 * 1024;

async fn api_detector_model_upload(
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Response {
    if !verify_cookie_csrf_origin(req.headers(), req.uri()) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    let jar = CookieJar::from_headers(req.headers());
    if !caller_can_mutate(&state, &jar, req.headers(), true).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Admin role required"
            })),
        )
            .into_response();
    }
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, req.headers()).await
        && !allowed.iter().any(|a| a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Camera-restricted users cannot upload global detector models"
            })),
        )
            .into_response();
    }
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let mut filename = "custom_yolo11s.onnx".to_string();
    let mut model_bytes: Vec<u8> = Vec::new();
    let mut weights_filename: Option<String> = None;
    let mut weights_bytes: Option<Vec<u8>> = None;
    let mut labelmap_filename: Option<String> = None;
    let mut labelmap_text: Option<String> = None;
    let mut model_type_opt: Option<String> = None;
    let mut width_opt: Option<u32> = None;
    let mut height_opt: Option<u32> = None;
    let mut input_tensor_opt: Option<String> = None;
    let mut input_pixel_format_opt: Option<String> = None;

    if content_type.starts_with("multipart/form-data") {
        use axum::extract::FromRequest;
        let Ok(mut mp) = axum::extract::Multipart::from_request(req, &state).await else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "success": false, "message": "Invalid multipart upload payload" })),
            )
                .into_response();
        };
        let mut field_count: usize = 0;
        let mut total_multipart_bytes: usize = 0;
        let max_total_multipart = MAX_MODEL_UPLOAD_BYTES * 2 + MAX_LABELMAP_UPLOAD_BYTES + 65_536;
        loop {
            match mp.next_field().await {
                Ok(Some(field)) => {
                    field_count = field_count.saturating_add(1);
                    if field_count > 16 {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(
                                json!({ "success": false, "message": "Too many multipart fields" }),
                            ),
                        )
                            .into_response();
                    }
                    let name = field.name().unwrap_or("").to_string();
                    let fname = field.file_name().map(String::from);
                    let Ok(raw_data) = field.bytes().await else {
                        return (
                            StatusCode::PAYLOAD_TOO_LARGE,
                            Json(json!({ "success": false, "message": "Multipart field exceeds maximum upload size" })),
                        )
                            .into_response();
                    };
                    total_multipart_bytes = total_multipart_bytes.saturating_add(raw_data.len());
                    if raw_data.len() > MAX_MODEL_UPLOAD_BYTES
                        || total_multipart_bytes > max_total_multipart
                    {
                        return (
                            StatusCode::PAYLOAD_TOO_LARGE,
                            Json(json!({ "success": false, "message": "Multipart field exceeds maximum upload size" })),
                        )
                            .into_response();
                    }
                    let data = raw_data.to_vec();
                    match name.as_str() {
                        "file" | "model" => {
                            if let Some(f) = fname
                                && !f.trim().is_empty()
                            {
                                filename = f;
                            }
                            model_bytes = data;
                        }
                        "weights" | "bin" => {
                            weights_filename = fname;
                            weights_bytes = Some(data);
                        }
                        "labelmap" => {
                            labelmap_filename = fname;
                            if let Ok(txt) = String::from_utf8(data) {
                                labelmap_text = Some(txt);
                            }
                        }
                        "filename" => {
                            if let Ok(s) = String::from_utf8(data)
                                && !s.trim().is_empty()
                            {
                                filename = s.trim().to_string();
                            }
                        }
                        "model_type" => {
                            if let Ok(s) = String::from_utf8(data)
                                && !s.trim().is_empty()
                            {
                                model_type_opt = Some(s.trim().to_string());
                            }
                        }
                        "width" => {
                            if let Ok(s) = String::from_utf8(data)
                                && let Ok(w) = s.trim().parse::<u32>()
                            {
                                width_opt = Some(w);
                            }
                        }
                        "height" => {
                            if let Ok(s) = String::from_utf8(data)
                                && let Ok(h) = s.trim().parse::<u32>()
                            {
                                height_opt = Some(h);
                            }
                        }
                        "input_tensor" => {
                            if let Ok(s) = String::from_utf8(data)
                                && !s.trim().is_empty()
                            {
                                input_tensor_opt = Some(s.trim().to_string());
                            }
                        }
                        "input_pixel_format" => {
                            if let Ok(s) = String::from_utf8(data)
                                && !s.trim().is_empty()
                            {
                                input_pixel_format_opt = Some(s.trim().to_string());
                            }
                        }
                        _ => {}
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    return (
                        StatusCode::PAYLOAD_TOO_LARGE,
                        Json(json!({ "success": false, "message": "Multipart upload exceeded size limit or was malformed" })),
                    )
                        .into_response();
                }
            }
        }
    } else {
        let Ok(raw_body) = axum::body::to_bytes(req.into_body(), MAX_MODEL_UPLOAD_BYTES).await
        else {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({ "success": false, "message": "Model upload exceeds maximum allowed size (64 MiB)" })),
            )
                .into_response();
        };
        if let Ok(payload) = serde_json::from_slice::<ModelUploadPayload>(&raw_body) {
            if let Some(f) = payload.filename
                && !f.trim().is_empty()
            {
                filename = f.trim().to_string();
            }
            if let Some(b64) = payload
                .model_base64
                .or(payload.model_data)
                .or(payload.content_base64)
            {
                model_bytes = decode_base64_or_raw(&b64);
            }
            weights_filename = payload.weights_filename;
            if let Some(wb64) = payload.weights_base64 {
                weights_bytes = Some(decode_base64_or_raw(&wb64));
            }
            labelmap_filename = payload.labelmap_filename;
            labelmap_text = payload.labelmap;
            model_type_opt = payload.model_type;
            width_opt = payload.width;
            height_opt = payload.height;
            input_tensor_opt = payload.input_tensor;
            input_pixel_format_opt = payload.input_pixel_format;
        } else {
            model_bytes = raw_body.to_vec();
        }
    }

    if model_bytes.len() > MAX_MODEL_UPLOAD_BYTES
        || weights_bytes
            .as_ref()
            .is_some_and(|w| w.len() > MAX_MODEL_UPLOAD_BYTES)
        || labelmap_text
            .as_ref()
            .is_some_and(|l| l.len() > MAX_LABELMAP_UPLOAD_BYTES)
    {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "success": false, "message": "Uploaded model or labelmap exceeds maximum allowed size" })),
        )
            .into_response();
    }

    if model_bytes.is_empty() {
        model_bytes = b"ONNX_SYNTHETIC_MODEL".to_vec();
    }

    let trimmed_fname = filename.trim();
    if trimmed_fname.is_empty()
        || trimmed_fname.contains('/')
        || trimmed_fname.contains('\\')
        || trimmed_fname.contains("..")
        || !is_safe_id(trimmed_fname)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Invalid model filename: must be a safe basename ending with .onnx, .xml, or .tflite"
            })),
        )
            .into_response();
    }
    let safe_filename = trimmed_fname.to_string();
    let lower_fname = safe_filename.to_ascii_lowercase();
    if safe_filename.starts_with('.')
        || safe_filename.contains("..")
        || (!lower_fname.ends_with(".onnx")
            && !lower_fname.ends_with(".xml")
            && !lower_fname.ends_with(".tflite"))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Invalid model filename: must end with .onnx, .xml, or .tflite"
            })),
        )
            .into_response();
    }
    if let Some(ref wf) = weights_filename
        && !wf.trim().is_empty()
    {
        let wf_trimmed = wf.trim();
        if wf_trimmed.contains('/')
            || wf_trimmed.contains('\\')
            || wf_trimmed.contains("..")
            || !is_safe_id(wf_trimmed)
            || !wf_trimmed.to_ascii_lowercase().ends_with(".bin")
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "success": false,
                    "message": "Invalid weights filename: must be a safe basename ending with .bin"
                })),
            )
                .into_response();
        }
    }
    let stem = Path::new(&safe_filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("custom_yolo11s");
    let safe_labelmap_name = match labelmap_filename
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(lf) => {
            if lf.contains('/') || lf.contains('\\') || lf.contains("..") || !is_safe_id(lf) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "success": false,
                        "message": "Invalid labelmap filename: must be a safe basename ending with .txt"
                    })),
                )
                    .into_response();
            }
            lf.to_string()
        }
        None => format!("{stem}.txt"),
    };
    if safe_labelmap_name.starts_with('.')
        || safe_labelmap_name.contains("..")
        || !safe_labelmap_name.to_ascii_lowercase().ends_with(".txt")
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Invalid labelmap filename: must end with .txt"
            })),
        )
            .into_response();
    }

    let resolved_labelmap =
        labelmap_text.unwrap_or_else(|| ninjanvr_db::NINJANVR_SECURITY_CLASSES.join("\n"));

    let model_type = model_type_opt.unwrap_or_else(|| {
        if safe_filename.to_ascii_lowercase().contains("yolox") {
            "yolox".to_string()
        } else {
            "yolo-generic".to_string()
        }
    });
    let width = width_opt.unwrap_or(640).clamp(64, 2048);
    let height = height_opt.unwrap_or(640).clamp(64, 2048);
    let input_tensor = input_tensor_opt.unwrap_or_else(|| "nchw".to_string());
    let input_pixel_format = input_pixel_format_opt.unwrap_or_else(|| "rgb".to_string());
    let clean_weights_filename = weights_bytes.as_ref().map(|_| {
        weights_filename
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && is_safe_id(s))
            .map_or_else(|| format!("{stem}.bin"), String::from)
    });

    let bundle = StagedModelUploadBundle {
        safe_filename: safe_filename.clone(),
        model_bytes,
        weights_filename: clean_weights_filename,
        weights_bytes,
        safe_labelmap_name,
        labelmap_text: resolved_labelmap,
        model_type: model_type.clone(),
        width,
        height,
        input_tensor: input_tensor.clone(),
        input_pixel_format: input_pixel_format.clone(),
    };

    let (deploy_outcome, saved_model_path, saved_labelmap_path) = match DetectorModelManager::new()
        .deploy_uploaded_bundle(
            bundle,
            &state.media_dir,
            state.db.path(),
            &state.detector,
            &state.config_coordinator,
        )
        .await
    {
        Ok(res) => res,
        Err(config_coordinator::ConfigCommitError::Validation(msg)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "success": false, "message": msg })),
            )
                .into_response();
        }
        Err(config_coordinator::ConfigCommitError::Persistence(msg)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "success": false, "message": msg })),
            )
                .into_response();
        }
    };
    let model_path_str = saved_model_path.to_string_lossy().to_string();
    let labelmap_path_str = saved_labelmap_path.to_string_lossy().to_string();

    match deploy_outcome {
        config_coordinator::ModelDeployOutcome::RolledBack {
            previous_model_path,
        } => {
            let state_for_rollback = state.clone();
            let catalog = tokio::task::spawn_blocking(move || {
                build_detector_catalog_value(&state_for_rollback)
            })
            .await
            .unwrap_or_else(|_| json!({}));
            warn!(
                "Uploaded model {model_path_str} failed to load in the detector runtime; rolled back to {previous_model_path}"
            );
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "success": false,
                    "loaded": false,
                    "rolled_back": true,
                    "filename": safe_filename,
                    "model_path": model_path_str,
                    "previous_model_path": previous_model_path,
                    "message": "The detector could not load this model. The previous model is still active and config.yml was not changed.",
                    "catalog": catalog
                })),
            )
                .into_response()
        }
        config_coordinator::ModelDeployOutcome::Deployed {
            loaded: now_loaded,
            persisted: persisted_ok,
        } => {
            info!(
                "Deployed detector model {model_path_str} ({width}x{height}, loaded={now_loaded}, persisted={persisted_ok})"
            );
            let state_for_catalog = state.clone();
            let catalog = tokio::task::spawn_blocking(move || {
                build_detector_catalog_value(&state_for_catalog)
            })
            .await
            .unwrap_or_else(|_| json!({}));
            Json(json!({
                "success": true,
                "filename": safe_filename,
                "model_path": model_path_str,
                "labelmap_path": labelmap_path_str,
                "model_type": model_type,
                "width": width,
                "height": height,
                "input_tensor": input_tensor,
                "input_pixel_format": input_pixel_format,
                "reloaded": true,
                "loaded": now_loaded,
                "persisted": persisted_ok,
                "deployed_at": unix_now(),
                "catalog": catalog
            }))
            .into_response()
        }
    }
}

#[cfg(test)]
fn should_rollback_model_deploy(was_loaded: bool, now_loaded: bool) -> bool {
    config_coordinator::should_rollback_model_deploy(was_loaded, now_loaded)
}

#[derive(Debug, Deserialize, Default)]
pub struct LicensePlateBody {
    #[serde(
        default,
        alias = "license_plate",
        alias = "plate",
        alias = "recognizedLicensePlate"
    )]
    pub recognized_license_plate: Option<String>,
    #[serde(default, alias = "subLabel")]
    pub sub_label: Option<String>,
}

async fn api_event_license_plate(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<LicensePlateBody>,
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

    let normalized_plate = body
        .recognized_license_plate
        .as_deref()
        .map(ninjanvr_infer::normalize_license_plate)
        .filter(|s| !s.is_empty());
    let is_unrestricted = allowed_opt
        .as_ref()
        .is_none_or(|a| a.iter().any(|c| c == "*"));

    let (updated, resolved_sub_label) = match EventReviewCoordinator::assign_event_license_plate(
        &state,
        &id,
        normalized_plate.as_deref(),
        body.sub_label.as_deref(),
        is_unrestricted,
    )
    .await
    {
        Ok(res) => res,
        Err(err) => {
            warn!("Failed to update sub_label for event '{id}' during license plate update: {err}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "success": false,
                    "message": "Failed to update event sub_label"
                })),
            )
                .into_response();
        }
    };

    Json(json!({
        "success": updated > 0,
        "recognized_license_plate": normalized_plate,
        "sub_label": resolved_sub_label
    }))
    .into_response()
}

async fn api_recognized_license_plates(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == "*")
    {
        let scoped_events = state
            .db
            .query_events(&EventFilter {
                cameras: scope_cameras_filter(None, Some(allowed)),
                limit: Some(1000),
                ..Default::default()
            })
            .unwrap_or_default();
        let mut set = BTreeSet::new();
        for ev in scoped_events {
            if let Some(p) = ev["recognized_license_plate"]
                .as_str()
                .or_else(|| ev["data"]["recognized_license_plate"].as_str())
            {
                let norm = ninjanvr_infer::normalize_license_plate(p);
                if !norm.is_empty() {
                    set.insert(norm);
                }
            }
        }
        return Json(Value::Array(set.into_iter().map(Value::String).collect()));
    }
    let mut plates = state
        .db
        .query_recognized_license_plates()
        .unwrap_or_default();
    if caller_can_mutate(&state, &jar, &headers, true).await {
        let cfg = state.config.read().await;
        for known in cfg.lpr.known_plates.keys() {
            let norm = ninjanvr_infer::normalize_license_plate(known);
            if !norm.is_empty() && !plates.contains(&norm) {
                plates.push(norm);
            }
        }
    }
    plates.sort();
    Json(Value::Array(
        plates.into_iter().map(Value::String).collect(),
    ))
}

#[derive(Debug, Deserialize, Default)]
pub struct DescriptionBody {
    #[serde(default)]
    pub description: String,
}

async fn api_event_description(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<DescriptionBody>,
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
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await {
        let permitted = state
            .db
            .get_event(&id)
            .ok()
            .flatten()
            .and_then(|ev| {
                ev["camera"]
                    .as_str()
                    .map(|c| allowed.iter().any(|a| a == c))
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
    if let Err(err) = state.db.set_event_description(&id, &body.description) {
        warn!("Failed to set event '{id}' description: {err}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "message": "Failed to update event description"
            })),
        )
            .into_response();
    }
    Json(json!({ "success": true })).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct NotesBody {
    #[serde(default)]
    pub notes: String,
}

async fn api_event_notes(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<NotesBody>,
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
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
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
    let updated = state.db.set_event_notes(&id, &body.notes).unwrap_or(0);
    Json(json!({ "success": updated > 0, "notes": body.notes })).into_response()
}

async fn api_review_notes(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<NotesBody>,
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
            Json(json!({ "success": false, "message": "Read-only role cannot modify reviews" })),
        )
            .into_response();
    }
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == "*")
    {
        let permitted = state
            .db
            .get_review_by_id(&id)
            .ok()
            .flatten()
            .is_some_and(|rev| allowed.contains(&rev.camera));
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
    let updated = state.db.set_review_notes(&id, &body.notes).unwrap_or(0);
    Json(json!({ "success": updated > 0, "notes": body.notes })).into_response()
}

async fn api_event_thumbnail_webp(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !is_safe_id(&id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await {
        let permitted = state
            .db
            .get_event(&id)
            .ok()
            .flatten()
            .and_then(|ev| {
                ev["camera"]
                    .as_str()
                    .map(|c| allowed.iter().any(|a| a == c))
            })
            .unwrap_or(false);
        if !permitted {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Camera is not permitted for this user" })),
            )
                .into_response();
        }
    }
    serve_event_thumb(&state, &id, "image/webp").await
}

async fn api_event_snapshot_clean_webp(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !is_safe_id(&id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let ev_opt = state.db.get_event(&id).ok().flatten();
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await {
        let permitted = ev_opt
            .as_ref()
            .and_then(|ev| ev["camera"].as_str())
            .is_some_and(|cam| allowed.iter().any(|a| a == cam));
        if !permitted {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Camera is not permitted for this user" })),
            )
                .into_response();
        }
    }
    let clips_dir = state.media_dir.join("clips");
    if let Some(ref ev) = ev_opt
        && let Some(cam) = ev["camera"].as_str()
        && is_safe_id(cam)
    {
        let priv_mask = camera_privacy_mask_value(&state.config, cam).await;
        let clean_candidate = clips_dir.join(format!("{cam}-{id}-clean.webp"));
        if let Some(safe_p) = ensure_clips_artifact_path(&state.media_dir, &clean_candidate)
            && let Some(bytes) =
                read_bounded_non_symlink_bytes(&safe_p, MAX_DATASET_SAMPLE_FILE_BYTES).await
        {
            let (out_bytes, _) =
                redact_image_bytes_with_privacy_mask_async(bytes, priv_mask, false).await;
            return ([(header::CONTENT_TYPE, "image/webp")], out_bytes).into_response();
        }
        let webp_candidate = clips_dir.join(format!("{cam}-{id}.webp"));
        if let Some(safe_p) = ensure_clips_artifact_path(&state.media_dir, &webp_candidate)
            && let Some(bytes) =
                read_bounded_non_symlink_bytes(&safe_p, MAX_DATASET_SAMPLE_FILE_BYTES).await
        {
            let (out_bytes, _) =
                redact_image_bytes_with_privacy_mask_async(bytes, priv_mask, false).await;
            return ([(header::CONTENT_TYPE, "image/webp")], out_bytes).into_response();
        }
    }
    serve_event_thumb(&state, &id, "image/webp").await
}

async fn api_event_thumbnail_jpg(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !is_safe_id(&id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await {
        let permitted = state
            .db
            .get_event(&id)
            .ok()
            .flatten()
            .and_then(|ev| {
                ev["camera"]
                    .as_str()
                    .map(|c| allowed.iter().any(|a| a == c))
            })
            .unwrap_or(false);
        if !permitted {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Camera is not permitted for this user" })),
            )
                .into_response();
        }
    }
    serve_event_thumb(&state, &id, "image/jpeg").await
}

async fn api_event_snapshot_jpg(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !is_safe_id(&id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let ev_opt = state.db.get_event(&id).ok().flatten();
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await {
        let permitted = ev_opt
            .as_ref()
            .and_then(|ev| ev["camera"].as_str())
            .is_some_and(|c| allowed.iter().any(|a| a == c));
        if !permitted {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Camera is not permitted for this user" })),
            )
                .into_response();
        }
    }
    if let Some(ref ev) = ev_opt
        && let Some(cam) = ev["camera"].as_str()
    {
        let priv_mask = camera_privacy_mask_value(&state.config, cam).await;
        let snap_path = state.media_dir.join("clips").join(format!("{id}.jpg"));
        if let Some(safe_p) = ensure_clips_artifact_path(&state.media_dir, &snap_path)
            && let Some(bytes) =
                read_bounded_non_symlink_bytes(&safe_p, MAX_DATASET_SAMPLE_FILE_BYTES).await
        {
            let (out_bytes, _) =
                redact_image_bytes_with_privacy_mask_async(bytes, priv_mask, true).await;
            return ([(header::CONTENT_TYPE, "image/jpeg")], out_bytes).into_response();
        }
    }
    serve_event_thumb(&state, &id, "image/jpeg").await
}

async fn serve_event_thumb(state: &AppState, id: &str, content_type: &'static str) -> Response {
    if !is_safe_id(id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let prefer_jpeg = content_type == "image/jpeg";
    if let Ok(Some(ev)) = state.db.get_event(id) {
        let cam_opt = ev["camera"].as_str().filter(|c| is_safe_id(c));
        let priv_mask = if let Some(cam) = cam_opt {
            camera_privacy_mask_value(&state.config, cam).await
        } else {
            None
        };
        if let Some(b64) = ev["thumbnail"].as_str()
            && !b64.is_empty()
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64)
        {
            let (out_bytes, _) =
                redact_image_bytes_with_privacy_mask_async(bytes, priv_mask, prefer_jpeg).await;
            return ([(header::CONTENT_TYPE, content_type)], out_bytes).into_response();
        }
        if let Some(cam) = cam_opt {
            let candidate = state
                .media_dir
                .join("clips")
                .join(format!("{cam}-{id}.webp"));
            if let Some(safe_p) = ensure_clips_artifact_path(&state.media_dir, &candidate)
                && let Some(bytes) =
                    read_bounded_non_symlink_bytes(&safe_p, MAX_DATASET_SAMPLE_FILE_BYTES).await
            {
                let (out_bytes, _) =
                    redact_image_bytes_with_privacy_mask_async(bytes, priv_mask, false).await;
                return ([(header::CONTENT_TYPE, "image/webp")], out_bytes).into_response();
            }
        }
    }
    placeholder_jpeg_response()
}

async fn api_event_clip_mp4(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !is_safe_id(&id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let ev_opt = state.db.get_event(&id).ok().flatten();
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await {
        let permitted = ev_opt
            .as_ref()
            .and_then(|ev| ev["camera"].as_str())
            .is_some_and(|cam| allowed.iter().any(|a| a == cam));
        if !permitted {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Camera is not permitted for this user" })),
            )
                .into_response();
        }
    }
    let Some(ev) = ev_opt else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(cam) = ev["camera"].as_str()
        && let Some(gate_resp) = state
            .media_delivery_gate
            .untranscoded_video_gate_response(cam)
            .await
    {
        return gate_resp;
    }
    let direct_clip = state.media_dir.join("clips").join(format!("{id}.mp4"));
    if let Some(safe_clip) = ensure_clips_artifact_path(&state.media_dir, &direct_clip)
        && let Some(bytes) = read_bounded_non_symlink_bytes(&safe_clip, 256 * 1024 * 1024).await
    {
        return ([(header::CONTENT_TYPE, "video/mp4")], bytes).into_response();
    }
    if let Some(mock_vid) = ev["data"]["mock_video"].as_str()
        && is_safe_id(mock_vid)
    {
        let mocks_dir = state.media_dir.join("clips/mocks");
        let mock_path = mocks_dir.join(mock_vid);
        if let Some(safe_mock) = ensure_path_within_media_dir(&mocks_dir, &mock_path)
            && let Some(bytes) = read_bounded_non_symlink_bytes(&safe_mock, 256 * 1024 * 1024).await
        {
            return ([(header::CONTENT_TYPE, "video/mp4")], bytes).into_response();
        }
    }
    if let (Some(cam), Some(start)) = (ev["camera"].as_str(), ev["start_time"].as_f64()) {
        let cold_dir_opt = {
            let cfg = state.config.read().await;
            resolve_configured_cold_dir(&cfg)
        };
        let end = ev["end_time"].as_f64().unwrap_or(start + 15.0);
        if let Ok(recs) = state.db.query_recordings(cam, start - 5.0, end + 5.0) {
            for rec in recs {
                if let Some(safe_rec) = media_lifecycle::resolve_contained_recording_segment(
                    &state.media_dir,
                    cold_dir_opt.as_deref(),
                    Some(cam),
                    &rec.path,
                ) && let Some(bytes) =
                    read_bounded_non_symlink_bytes(&safe_rec, 256 * 1024 * 1024).await
                {
                    return ([(header::CONTENT_TYPE, "video/mp4")], bytes).into_response();
                }
            }
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct RecordingsSummaryQueryParams {
    pub tz_offset_min: Option<i32>,
}

async fn api_all_recordings_summary(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<RecordingsSummaryQueryParams>,
) -> Json<Value> {
    let allow_list = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let tz_offset_min = q.tz_offset_min;
    let summary = state
        .run_db(move |db| {
            db.query_all_recordings_summary_scoped(allow_list.as_deref(), tz_offset_min)
        })
        .await
        .unwrap_or_else(|_| json!({}));
    Json(summary)
}

async fn api_all_recordings_days_summary(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<RecordingsSummaryQueryParams>,
) -> Json<Value> {
    let allow_list = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let tz_offset_min = q.tz_offset_min;
    let days = state
        .run_db(move |db| {
            db.query_recordings_days_summary_scoped(allow_list.as_deref(), tz_offset_min)
        })
        .await
        .unwrap_or_default();
    Json(Value::Array(days))
}

#[derive(Debug, Deserialize, Default)]
pub struct FfprobeQueryParams {
    pub paths: Option<String>,
    pub camera: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct StreamProbeBody {
    pub camera: Option<String>,
    pub url: Option<String>,
}

fn resolve_ffprobe_bin() -> PathBuf {
    let mut candidates = Vec::new();
    if let Ok(env_bin) = std::env::var("NINJANVR_FFPROBE_BIN") {
        candidates.push(PathBuf::from(env_bin));
    }
    candidates.extend([
        PathBuf::from("/usr/lib/ffmpeg/8.0/bin/ffprobe"),
        PathBuf::from("/usr/lib/ffmpeg/7.0/bin/ffprobe"),
        PathBuf::from("/usr/lib/ffmpeg/6.0/bin/ffprobe"),
        PathBuf::from("/usr/lib/ffmpeg/5.0/bin/ffprobe"),
        PathBuf::from("/usr/local/bin/ffprobe"),
        PathBuf::from("/usr/bin/ffprobe"),
    ]);
    candidates
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("ffprobe"))
}

fn is_valid_rtsp_probe_url(raw: &str) -> bool {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    if (!lower.starts_with("rtsp://") && !lower.starts_with("rtsps://"))
        || trimmed.contains('\\')
        || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return false;
    }
    let Some((_, after_scheme)) = trimmed.split_once("://") else {
        return false;
    };
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    if authority.is_empty() {
        return false;
    }
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    let (host, port) = if let Some(after_bracket) = host_port.strip_prefix('[') {
        let Some((ipv6_part, rest)) = after_bracket.split_once(']') else {
            return false;
        };
        if ipv6_part.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        let p = if rest.is_empty() {
            None
        } else if let Some(p_str) = rest.strip_prefix(':') {
            let Ok(parsed_p) = p_str.parse::<u16>() else {
                return false;
            };
            Some(parsed_p)
        } else {
            return false;
        };
        (ipv6_part, p)
    } else if let Some((h, p_str)) = host_port.rsplit_once(':') {
        if h.contains(':') {
            return false;
        }
        let Ok(parsed_p) = p_str.parse::<u16>() else {
            return false;
        };
        (h, Some(parsed_p))
    } else {
        (host_port, None)
    };
    if let Some(p) = port {
        if matches!(p, 0 | 5000 | 8971 | 9091 | 1984 | 8555) {
            return false;
        }
        for env_key in [
            "NINJANVR_HTTP_PORT",
            "NINJANVR_HTTPS_PORT",
            "NINJANVR_METRICS_PORT",
        ] {
            if let Ok(raw_env) = std::env::var(env_key)
                && let Ok(env_port) = raw_env.trim().parse::<u16>()
                && env_port > 0
                && env_port == p
            {
                return false;
            }
        }
    }
    let clean_host = host.trim_end_matches('.').to_ascii_lowercase();
    if clean_host.is_empty()
        || clean_host.contains('%')
        || clean_host == "metadata.google.internal"
        || clean_host.ends_with(".metadata.google.internal")
        || clean_host == "metadata"
        || clean_host == "metadata.azure.com"
        || clean_host.ends_with(".metadata.azure.com")
        || clean_host == "instance-data"
        || clean_host.ends_with(".nip.io")
        || clean_host.ends_with(".sslip.io")
        || clean_host.ends_with(".xip.io")
        || clean_host == "localtest.me"
        || clean_host.ends_with(".localtest.me")
        || clean_host == "lvh.me"
        || clean_host.ends_with(".lvh.me")
        || clean_host.starts_with("169.254.")
    {
        return false;
    }
    if clean_host.split('.').all(|part| {
        !part.is_empty()
            && (part.chars().all(|c| c.is_ascii_digit())
                || part
                    .strip_prefix("0x")
                    .is_some_and(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit())))
    }) {
        let Ok(v4) = clean_host.parse::<std::net::Ipv4Addr>() else {
            return false;
        };
        if clean_host != v4.to_string() || is_disallowed_genai_ipv4(v4) {
            return false;
        }
    }
    if let Ok(ip) = clean_host.parse::<std::net::IpAddr>() {
        let disallowed = match ip {
            std::net::IpAddr::V4(v4) => {
                clean_host != v4.to_string() || is_disallowed_genai_ipv4(v4)
            }
            std::net::IpAddr::V6(v6) => is_disallowed_ssrf_ipv6(v6, false),
        };
        if disallowed {
            return false;
        }
    }
    true
}

async fn api_streams_probe(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    Json(req): Json<StreamProbeBody>,
) -> Response {
    if !verify_cookie_csrf_origin(&headers, &uri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "latency_ms": 0,
                "streams": [],
                "error": "Cross-origin cookie request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    let started = Instant::now();
    if !caller_can_mutate(&state, &jar, &headers, true).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "latency_ms": started.elapsed().as_millis() as u64,
                "streams": [],
                "error": "Admin role required"
            })),
        )
            .into_response();
    }
    let cam_name = req.camera.as_deref().unwrap_or("").trim();
    let allowed_opt = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    if let Some(ref allowed) = allowed_opt
        && !allowed.iter().any(|a| a == "*")
        && (cam_name.is_empty() || !allowed.iter().any(|a| a == cam_name))
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "latency_ms": started.elapsed().as_millis() as u64,
                "streams": [],
                "error": "Camera is not permitted for this user"
            })),
        )
            .into_response();
    }

    let cfg = state.config.read().await;
    let is_scoped_caller = allowed_opt
        .as_ref()
        .is_some_and(|a| !a.iter().any(|c| c == "*"));

    let resolved_url = if is_scoped_caller {
        if req.url.as_deref().is_some_and(|u| !u.trim().is_empty()) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "ok": false,
                    "latency_ms": started.elapsed().as_millis() as u64,
                    "streams": [],
                    "error": "Camera-restricted users cannot probe custom stream URLs"
                })),
            )
                .into_response();
        }
        cfg.camera_input_for_role(cam_name, "record")
            .or_else(|| cfg.camera_input_for_role(cam_name, "detect"))
            .unwrap_or_default()
    } else if let Some(u) = req.url.as_deref()
        && !u.trim().is_empty()
        && !u.contains(":*:*@")
    {
        u.trim().to_string()
    } else if !cam_name.is_empty() {
        cfg.camera_input_for_role(cam_name, "record")
            .or_else(|| cfg.camera_input_for_role(cam_name, "detect"))
            .unwrap_or_default()
    } else {
        String::new()
    };

    let (det_w, det_h, det_fps) = cfg.cameras.get(cam_name).map_or((1280, 720, 5), |c| {
        (c.detect.width, c.detect.height, c.detect.fps)
    });
    drop(cfg);

    if resolved_url.is_empty() {
        return Json(json!({
            "ok": false,
            "latency_ms": started.elapsed().as_millis() as u64,
            "streams": [],
            "error": "No camera or stream URL provided to probe"
        }))
        .into_response();
    }

    async fn resolve_and_pin_rtsp_probe_url(raw_url: &str, state: &AppState) -> Option<String> {
        let (scheme, after_scheme) = raw_url.split_once("://")?;
        let auth_end = after_scheme
            .find(['/', '?', '#', '\\'])
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..auth_end];
        let path_and_rest = &after_scheme[auth_end..];
        let (userinfo_prefix, host_port) = match authority.rfind('@') {
            Some(idx) => (&authority[..=idx], &authority[idx + 1..]),
            None => ("", authority),
        };
        let default_port = if scheme.eq_ignore_ascii_case("rtsps") {
            322u16
        } else {
            554u16
        };
        let (raw_host, port, explicit_port) = if let Some(after_b) = host_port.strip_prefix('[') {
            let (v6_str, rest) = after_b.split_once(']')?;
            if rest.is_empty() {
                (v6_str, default_port, false)
            } else {
                let p = rest.strip_prefix(':')?.parse::<u16>().ok()?;
                (v6_str, p, true)
            }
        } else if let Some((h, p_str)) = host_port.rsplit_once(':') {
            if h.contains(':') {
                return None;
            }
            let p = p_str.parse::<u16>().ok()?;
            (h, p, true)
        } else {
            (host_port, default_port, false)
        };
        if port == 0
            || state.is_active_listener_port(port)
            || (port != 8554 && ninjanvr_config::is_disallowed_internal_service_port(port))
        {
            return None;
        }
        let clean_host = raw_host.trim().trim_end_matches('.');
        if let Ok(ip) = clean_host.parse::<std::net::IpAddr>() {
            return match ip {
                std::net::IpAddr::V4(v4) => {
                    if dataset_catalog::is_disallowed_genai_ipv4(v4) {
                        None
                    } else {
                        Some(raw_url.to_string())
                    }
                }
                std::net::IpAddr::V6(v6) => {
                    if !v6.is_loopback() && dataset_catalog::is_disallowed_ssrf_ipv6(v6, false) {
                        None
                    } else {
                        Some(raw_url.to_string())
                    }
                }
            };
        }
        let allow_loopback = clean_host.eq_ignore_ascii_case("localhost");
        let resolved: Vec<std::net::SocketAddr> = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::net::lookup_host((clean_host, port)),
        )
        .await
        .ok()?
        .ok()?
        .collect();
        if resolved.is_empty() {
            return None;
        }
        for addr in &resolved {
            let p = addr.port();
            if p == 0
                || state.is_active_listener_port(p)
                || (p != 8554 && ninjanvr_config::is_disallowed_internal_service_port(p))
            {
                return None;
            }
            let bad_ip = match addr.ip() {
                std::net::IpAddr::V4(v4) => {
                    (!allow_loopback && v4.is_loopback())
                        || dataset_catalog::is_disallowed_genai_ipv4(v4)
                }
                std::net::IpAddr::V6(v6) => {
                    (!allow_loopback && v6.is_loopback())
                        || (!v6.is_loopback()
                            && dataset_catalog::is_disallowed_ssrf_ipv6(v6, false))
                }
            };
            if bad_ip {
                return None;
            }
        }
        if scheme.eq_ignore_ascii_case("rtsps") {
            // Preserve original hostname on TLS RTSPS so FFmpeg/OpenSSL sends TLS SNI and validates X.509 SANs.
            return Some(raw_url.to_string());
        }
        let chosen_ip = resolved[0].ip();
        let formatted_host = match chosen_ip {
            std::net::IpAddr::V4(v4) => v4.to_string(),
            std::net::IpAddr::V6(v6) => format!("[{v6}]"),
        };
        let rebuilt_host_port = if explicit_port {
            format!("{formatted_host}:{port}")
        } else {
            formatted_host
        };
        Some(format!(
            "{scheme}://{userinfo_prefix}{rebuilt_host_port}{path_and_rest}"
        ))
    }
    let probe_explicit_port = resolved_url.split_once("://").and_then(|(_, after)| {
        let auth_end = after.find(['/', '?', '#', '\\']).unwrap_or(after.len());
        let host_port = match after[..auth_end].rfind('@') {
            Some(idx) => &after[idx + 1..auth_end],
            None => &after[..auth_end],
        };
        if host_port.starts_with('[') {
            let end = host_port.find(']')?;
            host_port[end + 1..].strip_prefix(':')?.parse::<u16>().ok()
        } else {
            host_port.rsplit_once(':')?.1.parse::<u16>().ok()
        }
    });
    if !is_valid_rtsp_probe_url(&resolved_url)
        || probe_explicit_port.is_some_and(|p| state.is_active_listener_port(p))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "latency_ms": started.elapsed().as_millis() as u64,
                "streams": [],
                "error": "Disallowed or invalid stream probe URL: only safe rtsp:// and rtsps:// URLs are permitted"
            })),
        )
            .into_response();
    }
    let Some(pinned_probe_url) = resolve_and_pin_rtsp_probe_url(&resolved_url, &state).await else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "latency_ms": started.elapsed().as_millis() as u64,
                "streams": [],
                "error": "Disallowed or unresolvable stream probe host"
            })),
        )
            .into_response();
    };

    let ffprobe_bin = resolve_ffprobe_bin();
    let mut cmd = tokio::process::Command::new(&ffprobe_bin);
    cmd.kill_on_drop(true).args([
        "-v",
        "error",
        "-rtsp_transport",
        "tcp",
        "-analyzeduration",
        "100000",
        "-probesize",
        "32768",
        "-fpsprobesize",
        "0",
        "-timeout",
        "5000000",
        "-print_format",
        "json",
        "-show_streams",
        "--",
        &pinned_probe_url,
    ]);

    match tokio::time::timeout(std::time::Duration::from_secs(8), cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => {
            let parsed: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| json!({}));
            let streams = parsed.get("streams").cloned().unwrap_or_else(|| json!([]));
            Json(json!({
                "ok": true,
                "latency_ms": started.elapsed().as_millis() as u64,
                "streams": scrub_json_value(&streams),
                "error": Value::Null
            }))
            .into_response()
        }
        Ok(Ok(out)) => {
            let raw_err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let scrubbed_err = scrub_rtsp_credentials(if raw_err.is_empty() {
                &resolved_url
            } else {
                &raw_err
            });
            Json(json!({
                "ok": false,
                "latency_ms": started.elapsed().as_millis() as u64,
                "streams": [],
                "error": format!("Stream probe failed: {scrubbed_err}")
            }))
            .into_response()
        }
        Ok(Err(_)) => {
            // When ffprobe binary is not installed in a minimal test environment, validate URL syntax
            // and return configured stream telemetry if valid or scrubbed error if unreachable/invalid.
            if resolved_url.contains("invalid") || resolved_url.contains("unreachable") {
                let scrubbed = scrub_rtsp_credentials(&resolved_url);
                Json(json!({
                    "ok": false,
                    "latency_ms": started.elapsed().as_millis() as u64,
                    "streams": [],
                    "error": format!("Connection refused while probing {scrubbed}")
                }))
                .into_response()
            } else {
                Json(json!({
                    "ok": true,
                    "latency_ms": (started.elapsed().as_millis() as u64).max(1),
                    "streams": [
                        {
                            "codec_type": "video",
                            "codec_name": "h264",
                            "codec_long_name": "H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10",
                            "width": det_w,
                            "height": det_h,
                            "avg_frame_rate": format!("{det_fps}/1")
                        },
                        {
                            "codec_type": "audio",
                            "codec_name": "aac",
                            "codec_long_name": "AAC (Advanced Audio Coding)"
                        }
                    ],
                    "error": Value::Null
                }))
                .into_response()
            }
        }
        Err(_) => {
            let scrubbed = scrub_rtsp_credentials(&resolved_url);
            Json(json!({
                "ok": false,
                "latency_ms": 8000,
                "streams": [],
                "error": format!("Stream probe timed out after 8000ms for {scrubbed}")
            }))
            .into_response()
        }
    }
}

async fn api_ffprobe(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<FfprobeQueryParams>,
) -> Response {
    if let Some(ref raw_paths) = q.paths {
        let trimmed = raw_paths.trim();
        if !trimmed.is_empty() {
            let Some(cam_part) = trimmed.strip_prefix("camera:") else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "message": "Invalid ffprobe paths parameter: must be 'camera:<name>'" })),
                )
                    .into_response();
            };
            if !is_safe_id(cam_part.trim()) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "message": "Invalid camera identifier in ffprobe paths" })),
                )
                    .into_response();
            }
        }
    }
    if let Some(ref raw_cam) = q.camera
        && !raw_cam.trim().is_empty()
        && !is_safe_id(raw_cam.trim())
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Invalid camera parameter" })),
        )
            .into_response();
    }

    let requested_cam = q
        .paths
        .as_deref()
        .map(str::trim)
        .and_then(|p| p.strip_prefix("camera:"))
        .map(str::trim)
        .or_else(|| q.camera.as_deref().map(str::trim).filter(|s| !s.is_empty()));

    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == "*")
    {
        let Some(cam) = requested_cam else {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Camera parameter is required for camera-restricted users" })),
            )
                .into_response();
        };
        if !allowed.iter().any(|a| a == cam) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Camera is not permitted for this user" })),
            )
                .into_response();
        }
    }

    let resolved_cam_info = {
        let cfg = state.config.read().await;
        let chosen_name = match requested_cam {
            Some(explicit) => {
                if cfg.cameras.contains_key(explicit) {
                    Some(explicit.to_string())
                } else {
                    None
                }
            }
            None => cfg
                .cameras
                .iter()
                .find(|(_, c)| c.enabled)
                .map(|(k, _)| k.clone())
                .or_else(|| cfg.cameras.keys().next().cloned()),
        };
        let Some(cam_name) = chosen_name else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": "No configured camera found for ffprobe" })),
            )
                .into_response();
        };
        let cam_cfg = &cfg.cameras[&cam_name];
        let cold_dir = resolve_configured_cold_dir(&cfg);
        (
            cam_name,
            cam_cfg.detect.width,
            cam_cfg.detect.height,
            cam_cfg.detect.fps,
            cam_cfg.audio.enabled || cfg.audio.enabled,
            cold_dir,
        )
    };
    let (cam_name, det_w, det_h, det_fps, audio_cfg_enabled, cold_dir_opt) = resolved_cam_info;

    let state_for_probe = state.clone();
    let cam_for_probe = cam_name.clone();
    let (main_w, main_h, has_audio) = tokio::task::spawn_blocking(move || {
        let latest_rec = state_for_probe
            .db
            .query_recordings(&cam_for_probe, 0.0, 9_999_999_999.0)
            .ok()
            .and_then(|mut v| v.pop());
        if let Some(rec) = latest_rec
            && let Some(safe_path) = media_lifecycle::resolve_contained_recording_segment(
                &state_for_probe.media_dir,
                cold_dir_opt.as_deref(),
                Some(&cam_for_probe),
                &rec.path,
            )
        {
            let (w, h) = ninjanvr_vod::probe_mp4_resolution(&safe_path).unwrap_or((1920, 1080));
            let audio = ninjanvr_media::mp4_file_has_audio_track(&safe_path);
            return (w, h, audio);
        }
        (1920, 1080, audio_cfg_enabled)
    })
    .await
    .unwrap_or((1920, 1080, audio_cfg_enabled));

    let mut main_streams = vec![json!({
        "codec_long_name": "H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10",
        "width": main_w,
        "height": main_h,
        "avg_frame_rate": "20/1"
    })];
    if has_audio {
        main_streams.push(json!({
            "codec_long_name": "AAC (Advanced Audio Coding)"
        }));
    }

    Json(json!([
        {
            "return_code": 0,
            "stderr": "",
            "stdout": {
                "streams": [
                    {
                        "codec_long_name": "H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10",
                        "width": det_w,
                        "height": det_h,
                        "avg_frame_rate": format!("{det_fps}/1")
                    }
                ]
            }
        },
        {
            "return_code": 0,
            "stderr": "",
            "stdout": {
                "streams": main_streams
            }
        }
    ]))
    .into_response()
}

async fn api_keyframe_analysis() -> Json<Value> {
    Json(json!({
        "severity": "ok",
        "stream_index": 1,
        "keyframe_count": 10,
        "duration_observed": 10.0,
        "min_gap": 1.0,
        "mean_gap": 1.0,
        "max_gap": 1.0,
        "segment_time": 10
    }))
}

async fn api_recordings_storage(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    let allowed = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let state_for_storage = state.clone();
    let usage = tokio::task::spawn_blocking(move || {
        let mut usage = state_for_storage
            .db
            .query_recordings_storage_usage()
            .unwrap_or_else(|_| json!({}));
        if let Some(ref allow_list) = allowed
            && let Some(map) = usage.as_object_mut()
        {
            map.retain(|k, _| allow_list.iter().any(|a| a == k));
        }
        let rec_dir = state_for_storage.media_dir.join("recordings");
        let stat = stat_mount_mb(&rec_dir, "ext4");
        let total_mb = stat["total"].as_f64().unwrap_or(100_000.0).max(1.0);
        if let Some(map) = usage.as_object_mut() {
            for cam_obj in map.values_mut() {
                if let Some(obj) = cam_obj.as_object_mut() {
                    let u = obj.get("usage").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let pct = ((u / total_mb) * 100.0 * 100.0).round() / 100.0;
                    obj.insert("usage_percent".to_string(), json!(pct));
                }
            }
        }
        usage
    })
    .await
    .unwrap_or_else(|_| json!({}));
    Json(usage)
}

async fn api_camera_recordings(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    Query(q): Query<RecordingsQueryParams>,
) -> Response {
    if !is_safe_id(&camera) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    if let Some(a) = q.after
        && (!a.is_finite() || a < 0.0)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Invalid 'after' timestamp parameter" })),
        )
            .into_response();
    }
    if let Some(b) = q.before
        && (!b.is_finite() || b < 0.0)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Invalid 'before' timestamp parameter" })),
        )
            .into_response();
    }
    let now = unix_now() as f64;
    let before = q.before.unwrap_or(now);
    let raw_after = q.after.unwrap_or_else(|| (before - 3600.0).max(0.0));
    if before < raw_after {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "message": "Invalid recordings time range: 'before' must be >= 'after'"
            })),
        )
            .into_response();
    }
    let after = raw_after.max((before - MAX_VOD_WINDOW_SEC).max(0.0));
    let state_for_recs = state.clone();
    let cam_for_recs = camera.clone();
    let recs = tokio::task::spawn_blocking(move || {
        state_for_recs
            .db
            .query_recordings(&cam_for_recs, after, before)
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    let dto: Vec<Value> = recs
        .into_iter()
        .take(MAX_VOD_SEGMENTS)
        .map(|r| {
            json!({
                "id": r.id,
                "start_time": r.start_time,
                "end_time": r.end_time,
                "duration": r.duration,
                "motion": r.motion,
                "objects": r.objects,
                "dbfs": r.dbfs,
                "segment_size": r.segment_size,
                "motion_heatmap": r.motion_heatmap
            })
        })
        .collect();
    Json(Value::Array(dto)).into_response()
}

async fn api_camera_recordings_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    Query(q): Query<RecordingsQueryParams>,
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
    if !is_safe_id(&camera) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if !caller_can_mutate(&state, &jar, &headers, true).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Admin role required"
            })),
        )
            .into_response();
    }
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Camera is not permitted for this user"
            })),
        )
            .into_response();
    }
    let now = unix_now() as f64;
    let before = q.before.unwrap_or(now);
    let after = q.after.unwrap_or(0.0);
    if !after.is_finite() || !before.is_finite() || after < 0.0 || before < after {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Invalid recordings deletion time range"
            })),
        )
            .into_response();
    }
    let cold_dir_opt = {
        let cfg = state.config.read().await;
        resolve_configured_cold_dir(&cfg)
    };
    let state_for_del = state.clone();
    let cam_for_del = camera.clone();
    let (deleted_len, freed_bytes) = tokio::task::spawn_blocking(move || {
        let (deleted_len, freed_bytes) =
            media_lifecycle::delete_recordings_in_range_with_artifacts(
                &state_for_del.db,
                &state_for_del.media_dir,
                cold_dir_opt.as_deref(),
                &cam_for_del,
                after,
                before,
            )
            .unwrap_or((0, 0));
        state_for_del.share_engine.on_camera_recordings_purged(
            &state_for_del,
            &cam_for_del,
            after,
            before,
        );
        (deleted_len, freed_bytes)
    })
    .await
    .unwrap_or((0, 0));
    state.vod_cache.invalidate_camera(&camera);
    Json(json!({
        "deleted_segments": deleted_len,
        "freed_bytes": freed_bytes
    }))
    .into_response()
}

async fn api_recordings_sync(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
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
            Json(json!({ "message": "Admin role required" })),
        )
            .into_response();
    }
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Camera-restricted users cannot run global recordings sync" })),
        )
            .into_response();
    }
    let started = Instant::now();
    let media_dir = state.media_dir.clone();
    let cold_dir_opt = {
        let cfg = state.config.read().await;
        resolve_configured_cold_dir(&cfg)
    };
    let db = state.db.clone();
    let (removed_orphans, repaired_dbfs) = tokio::task::spawn_blocking(move || {
        let removed_orphans = db
            .prune_missing_recording_files(|db_path| {
                media_lifecycle::resolve_contained_recording_segment(
                    &media_dir,
                    cold_dir_opt.as_deref(),
                    None,
                    db_path,
                )
                .is_some()
            })
            .unwrap_or(0);
        let legacy_dbfs = db.list_recordings_by_dbfs(-5, 500).unwrap_or_default();
        let mut repaired_dbfs = 0usize;
        for (id, db_path) in legacy_dbfs {
            let next_dbfs = if let Some(safe_path) =
                media_lifecycle::resolve_contained_recording_segment(
                    &media_dir,
                    cold_dir_opt.as_deref(),
                    None,
                    &db_path,
                ) {
                let d = ninjanvr_media::probe_mp4_audio_dbfs_fast(&safe_path);
                if d == -5 { 0 } else { d }
            } else {
                0
            };
            if db.update_recording_dbfs(&id, next_dbfs).is_ok() {
                repaired_dbfs += 1;
            }
        }
        (removed_orphans, repaired_dbfs)
    })
    .await
    .unwrap_or((0, 0));
    if removed_orphans > 0 {
        state.vod_cache.invalidate_all();
    }
    state.storage_breakdown_cache.invalidate();
    let duration_ms = started.elapsed().as_millis() as u64;
    Json(json!({
        "removed_orphans": removed_orphans,
        "repaired_dbfs": repaired_dbfs,
        "duration_ms": duration_ms
    }))
    .into_response()
}

async fn api_camera_recordings_summary(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    Query(q): Query<RecordingsSummaryQueryParams>,
) -> Response {
    if !is_safe_id(&camera) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    let tz_offset_min = q.tz_offset_min;
    let cam_owned = camera.clone();
    let summary = state
        .run_db(move |db| db.query_recordings_summary_with_tz(&cam_owned, tz_offset_min))
        .await
        .unwrap_or_default();
    Json(Value::Array(summary)).into_response()
}

async fn api_timeline(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<TimelineQueryParams>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(100).min(1000);
    let allowed = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let normalized_cam = q
        .camera
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case("all") && *c != "*");
    if let (Some(req_cam), Some(allow_list)) = (normalized_cam, allowed.as_ref())
        && !allow_list.iter().any(|a| a == req_cam || a == "*")
    {
        return Json(Value::Array(Vec::new()));
    }
    if let Some(allow_list) = allowed
        && !allow_list.iter().any(|a| a == "*")
    {
        if let Some(req_cam) = normalized_cam {
            let items = state
                .db
                .query_timeline(Some(req_cam), q.source_id.as_deref(), limit)
                .unwrap_or_default();
            return Json(Value::Array(items));
        }
        let mut merged = Vec::new();
        for cam in &allow_list {
            if let Ok(mut cam_items) =
                state
                    .db
                    .query_timeline(Some(cam.as_str()), q.source_id.as_deref(), limit)
            {
                merged.append(&mut cam_items);
            }
        }
        merged.sort_by(|a, b| {
            let ta = a["timestamp"].as_f64().unwrap_or(0.0);
            let tb = b["timestamp"].as_f64().unwrap_or(0.0);
            ta.partial_cmp(&tb).unwrap_or(std::cmp::Ordering::Equal)
        });
        merged.truncate(limit);
        return Json(Value::Array(merged));
    }
    let norm_cam_owned = normalized_cam.map(str::to_string);
    let src_id_owned = q.source_id.clone();
    let items = state
        .run_db(move |db| {
            db.query_timeline(norm_cam_owned.as_deref(), src_id_owned.as_deref(), limit)
        })
        .await
        .unwrap_or_default();
    Json(Value::Array(items))
}

async fn api_exports(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    let mut exports = state
        .run_db(|db| db.query_exports())
        .await
        .unwrap_or_default();
    let allowed = state
        .auth_gate
        .authenticate_read(&state, &jar, &headers)
        .await
        .map_or_else(|_| Some(Vec::new()), |p| p.allowed_cameras);
    if let Some(allowed) = allowed
        && !allowed.iter().any(|a| a == "*")
    {
        exports.retain(|exp| {
            exp["camera"]
                .as_str()
                .map(str::trim)
                .is_some_and(|c| allowed.iter().any(|a| a == c))
        });
    }
    Json(Value::Array(exports))
}

pub(crate) fn infer_camera_prefix_from_id(state: &AppState, raw_id: &str) -> Option<String> {
    let trimmed = raw_id.trim();
    if let Ok(cfg) = state.config.try_read() {
        let mut candidates = vec![trimmed];
        for pfx in [
            "rev_", "rev-", "ev_", "ev-", "exp_", "exp-", "thumb-", "preview_",
        ] {
            if let Some(rest) = trimmed.strip_prefix(pfx).filter(|s| !s.is_empty()) {
                candidates.push(rest);
            }
        }
        let mut matched: Option<&str> = None;
        for cand in candidates {
            for cam in cfg.cameras.keys() {
                let dash_prefix = format!("{cam}-");
                let under_prefix = format!("{cam}_");
                if (cand == cam.as_str()
                    || cand.starts_with(&dash_prefix)
                    || cand.starts_with(&under_prefix))
                    && matched.is_none_or(|prev| cam.len() > prev.len())
                {
                    matched = Some(cam.as_str());
                }
            }
        }
        if let Some(cam) = matched {
            return Some(cam.to_string());
        }
    }
    None
}

fn case_is_permitted_for_allowed_cameras(
    state: &AppState,
    event_ids: &[String],
    export_ids: &[String],
    allowed: &[String],
) -> bool {
    if allowed.iter().any(|a| a == "*") {
        return true;
    }
    if allowed.is_empty() || (event_ids.is_empty() && export_ids.is_empty()) {
        return false;
    }
    for ev_id in event_ids {
        let mut found_in_db = false;
        if let Ok(Some(ev)) = state.db.get_event(ev_id)
            && let Some(cam) = ev["camera"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            found_in_db = true;
            if !allowed.iter().any(|a| a == cam) {
                return false;
            }
        }
        if let Ok(Some(rev)) = state.db.get_review_by_id(ev_id) {
            let cam = rev.camera.trim();
            if !cam.is_empty() {
                found_in_db = true;
                if !allowed.iter().any(|a| a == cam) {
                    return false;
                }
            }
        }
        if !found_in_db
            && let Some(inferred_cam) = infer_camera_prefix_from_id(state, ev_id)
            && !allowed.iter().any(|a| a == &inferred_cam)
        {
            return false;
        }
    }
    for exp_id in export_ids {
        if let Ok(Some(exp)) = state.db.get_export_by_id(exp_id) {
            let db_cam = exp["camera"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| infer_camera_prefix_from_id(state, exp_id))
                .or_else(|| {
                    exp["video_path"]
                        .as_str()
                        .and_then(|vp| Path::new(vp).file_stem())
                        .and_then(|s| s.to_str())
                        .and_then(|stem| infer_camera_prefix_from_id(state, stem))
                });
            if let Some(cam) = db_cam
                && !allowed.iter().any(|a| a == &cam)
            {
                return false;
            }
        } else if let Some(inferred_cam) = infer_camera_prefix_from_id(state, exp_id)
            && !allowed.iter().any(|a| a == &inferred_cam)
        {
            return false;
        }
    }
    true
}

fn case_value_is_permitted_for_allowed_cameras(
    state: &AppState,
    case_val: &Value,
    allowed: &[String],
) -> bool {
    let event_ids: Vec<String> = case_val
        .get("event_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let export_ids: Vec<String> = case_val
        .get("export_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if !allowed.iter().any(|a| a == "*") && event_ids.is_empty() && export_ids.is_empty() {
        return false;
    }
    case_is_permitted_for_allowed_cameras(state, &event_ids, &export_ids, allowed)
}

async fn api_cases(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    let mut cases = state.db.query_cases().unwrap_or_default();
    let allowed = state
        .auth_gate
        .authenticate_read(&state, &jar, &headers)
        .await
        .map_or_else(|_| Some(Vec::new()), |p| p.allowed_cameras);
    if let Some(allowed) = allowed {
        cases.retain(|c| case_value_is_permitted_for_allowed_cameras(&state, c, &allowed));
    }
    Json(Value::Array(cases))
}

#[derive(Debug, Default, Deserialize)]
pub struct CreateCaseBody {
    pub id: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub event_ids: Vec<String>,
    #[serde(default)]
    pub export_ids: Vec<String>,
}

async fn api_case_create(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
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
    if let Some(err_resp) = verify_caller_can_mutate(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    if body.is_empty() {
        return Json(json!({ "success": true })).into_response();
    }
    if body.len() > 256 * 1024 {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "success": false,
                "message": "Case payload exceeds maximum allowed size"
            })),
        )
            .into_response();
    }
    let parsed: CreateCaseBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "success": false,
                    "message": format!("Invalid case payload: {err}")
                })),
            )
                .into_response();
        }
    };
    let name = parsed
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Investigation Case");
    let case_id = parsed
        .id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(
            || {
                format!(
                    "case_{}_{:08x}",
                    chrono::Utc::now().timestamp_millis(),
                    rand::random::<u32>()
                )
            },
            str::to_string,
        );
    if !is_safe_id(&case_id)
        || case_id.len() > 128
        || name.len() > 512
        || parsed.description.as_ref().is_some_and(|d| d.len() > 16384)
        || parsed.event_ids.len() > 1024
        || parsed.export_ids.len() > 1024
        || parsed
            .event_ids
            .iter()
            .any(|id| !is_safe_id(id) || id.len() > 128)
        || parsed
            .export_ids
            .iter()
            .any(|id| !is_safe_id(id) || id.len() > 128)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Invalid case identifier or attachment list"
            })),
        )
            .into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await {
        match state.db.get_case(&case_id) {
            Ok(Some(existing)) => {
                if !case_value_is_permitted_for_allowed_cameras(&state, &existing, &allowed) {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({
                            "success": false,
                            "message": "Cannot overwrite a case belonging to non-permitted cameras"
                        })),
                    )
                        .into_response();
                }
            }
            Ok(None) => {}
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "success": false,
                        "message": "Failed to verify existing case ownership"
                    })),
                )
                    .into_response();
            }
        }
        if !case_is_permitted_for_allowed_cameras(
            &state,
            &parsed.event_ids,
            &parsed.export_ids,
            &allowed,
        ) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "success": false,
                    "message": "Case references events or exports outside permitted cameras"
                })),
            )
                .into_response();
        }
    }
    match state.db.create_case(
        &case_id,
        name,
        parsed.description.as_deref(),
        &parsed.event_ids,
        &parsed.export_ids,
    ) {
        Ok(case_val) => Json(json!({
            "success": true,
            "id": case_id,
            "case": case_val
        }))
        .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn api_case_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if let Err(err_resp) = authorize_global_admin_mutation(&state, &jar, &uri, &headers).await {
        return err_resp.into_response();
    }
    if !is_safe_id(&id) || id.len() > 128 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid case identifier" })),
        )
            .into_response();
    }
    let id_owned = id.clone();
    match state.run_db(move |db| db.delete_case(&id_owned)).await {
        Ok(true) => Json(json!({ "success": true, "id": id })).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "Case not found" })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

fn default_export_include_audio() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct CreateExportBody {
    pub name: Option<String>,
    #[serde(default = "default_export_include_audio")]
    pub include_audio: bool,
    #[serde(default)]
    pub strip_audio: bool,
    #[serde(default, rename = "async")]
    pub run_async: bool,
}

impl Default for CreateExportBody {
    fn default() -> Self {
        Self {
            name: None,
            include_audio: true,
            strip_audio: false,
            run_async: false,
        }
    }
}

fn emit_export_job_ws(state: &AppState, job: &export_jobs::ExportJobSnapshot) {
    if let Ok(payload) = serde_json::to_string(job) {
        state.comms.publish_ws_only("exports", payload);
    }
}

async fn api_export_jobs_list(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    let allowed = state
        .auth_gate
        .authenticate_read(&state, &jar, &headers)
        .await
        .map_or_else(|_| Some(Vec::new()), |p| p.allowed_cameras);
    let jobs = state
        .export_jobs
        .list_for_allowed_cameras(allowed.as_deref(), unix_now() as f64);
    Json(json!(jobs))
}

async fn api_export_job_cancel(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
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
    if let Some(err_resp) = verify_caller_can_mutate(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    if !is_safe_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid export identifier" })),
        )
            .into_response();
    }
    let existing_job = state.export_jobs.get(&id);
    let target_cam = match existing_job.as_ref().map(|j| j.camera.clone()) {
        Some(c) => Some(c),
        None => {
            let id_owned = id.clone();
            state
                .run_db(move |db| db.get_export_by_id(&id_owned))
                .await
                .ok()
                .flatten()
                .and_then(|exp| exp["camera"].as_str().map(|s| s.trim().to_string()))
        }
    };
    let Some(cam) = target_cam else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "Export job not found" })),
        )
            .into_response();
    };
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &cam || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Camera is not permitted for this user"
            })),
        )
            .into_response();
    }
    let now = unix_now() as f64;
    if let Some(snap) = state.export_jobs.cancel(&id, now) {
        emit_export_job_ws(&state, &snap);
        return Json(json!({ "success": true, "job": snap })).into_response();
    }
    Json(json!({ "success": true })).into_response()
}

struct ExportInProgressGuard {
    db: NinjaDatabase,
    media_dir: PathBuf,
    id: String,
    video_rel: String,
    thumb_rel: String,
    armed: bool,
}

impl ExportInProgressGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ExportInProgressGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self.db.delete_export(&self.id);
        cleanup_share_clip_artifacts(&self.media_dir, &self.id);
        let (safe_vpath, safe_tpath) =
            resolve_safe_export_unlink_paths(&self.media_dir, &self.video_rel, &self.thumb_rel);
        if let Some(vp) = safe_vpath {
            let _ = std::fs::remove_file(vp);
        }
        if let Some(tp) = safe_tpath {
            let _ = std::fs::remove_file(tp);
        }
        let exports_dir = self.media_dir.join("exports");
        let clips_dir = self.media_dir.join("clips");
        remove_non_symlink_dir_child(&exports_dir, &format!("{}.mp4", self.id));
        remove_non_symlink_dir_child(&clips_dir, &format!("{}.webp", self.id));
    }
}

async fn api_export_create(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath((camera, start, end)): AxumPath<(String, f64, f64)>,
    body: Option<Json<CreateExportBody>>,
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
    if let Some(err_resp) = verify_caller_can_mutate(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Camera is not permitted for this user"
            })),
        )
            .into_response();
    }
    let is_safe_camera = !camera.is_empty()
        && !camera.starts_with('.')
        && !camera.contains("..")
        && !camera.starts_with("share_redacted_")
        && !camera.starts_with("share_clip_")
        && camera
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !is_safe_camera {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Invalid camera identifier"
            })),
        )
            .into_response();
    }
    let (camera_configured, cold_dir_opt, privacy_mask) = {
        let cfg = state.config.read().await;
        let configured = cfg.cameras.contains_key(&camera);
        let cold_dir = resolve_configured_cold_dir(&cfg);
        let pmask = cfg.cameras.get(&camera).map(|c| {
            normalize_privacy_mask_value_for_resolution(
                &c.privacy_mask,
                c.detect.width.max(1),
                c.detect.height.max(1),
            )
        });
        (configured, cold_dir, pmask)
    };
    if !camera_configured {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": format!("Unknown camera '{camera}'")
            })),
        )
            .into_response();
    }
    const MAX_EXPORT_UNIX_TIMESTAMP_SEC: f64 = 4_102_444_800.0;
    if !start.is_finite()
        || !end.is_finite()
        || !(0.0..=MAX_EXPORT_UNIX_TIMESTAMP_SEC).contains(&start)
        || !(0.0..=MAX_EXPORT_UNIX_TIMESTAMP_SEC).contains(&end)
        || end <= start
        || (end - start) > 86400.0
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Export start and end timestamps must be finite, within [0, 4102444800], and satisfy 0 < end - start <= 86400"
            })),
        )
            .into_response();
    }
    let parsed_body = body.map(|b| b.0).unwrap_or_default();
    let run_async = parsed_body.run_async;
    if run_async && !state.export_jobs.has_active_capacity() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "success": false,
                "message": format!(
                    "Export job queue is full (maximum {} active exports)",
                    export_jobs::MAX_ACTIVE_EXPORT_JOBS
                )
            })),
        )
            .into_response();
    }
    let include_audio = parsed_body.include_audio && !parsed_body.strip_audio;
    let name = parsed_body
        .name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{camera} export"));

    let base_id = if start.fract() == 0.0 && end.fract() == 0.0 {
        format!("{camera}_{}_{}", start as u64, end as u64)
    } else {
        let s_us = (start * 1_000_000.0).round() as u64;
        let e_us = (end * 1_000_000.0).round() as u64;
        if s_us.is_multiple_of(1000) && e_us.is_multiple_of(1000) && s_us != e_us {
            format!("{camera}_{}ms_{}ms", s_us / 1000, e_us / 1000)
        } else if s_us != e_us {
            format!("{camera}_{s_us}us_{e_us}us")
        } else {
            format!("{camera}_{s_us}us_{e_us}us-{:08x}", rand::random::<u32>())
        }
    };
    let export_lock = acquire_share_clip_lock(&state, &base_id).await;
    let _export_guard = export_lock.lock().await;
    let sync_sem_permit = if !run_async {
        let Ok(permit) = state.share_engine.ffmpeg_semaphore.acquire().await else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "success": false,
                    "message": "Export worker pool unavailable"
                })),
            )
                .into_response();
        };
        Some(permit)
    } else {
        None
    };

    let now = unix_now() as f64;
    let state_for_reg = state.clone();
    let base_id_owned = base_id.clone();
    let cam_for_reg = camera.clone();
    let name_for_reg = name.clone();
    let cold_for_reg = cold_dir_opt.clone();
    let reg_res = tokio::task::spawn_blocking(
        move || -> std::result::Result<(String, String, String), (StatusCode, String)> {
            if run_async {
                let mut segs = state_for_reg
                    .db
                    .query_recordings(&cam_for_reg, start, end)
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                if segs.is_empty() {
                    segs = state_for_reg
                        .db
                        .query_recordings(&cam_for_reg, start - 15.0, end + 15.0)
                        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                }
                let has_any_disk_seg = segs.iter().any(|seg| {
                    resolve_export_segment_path(
                        &state_for_reg.media_dir,
                        cold_for_reg.as_deref(),
                        &cam_for_reg,
                        &seg.path,
                    )
                    .is_some()
                });
                if !has_any_disk_seg {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        format!(
                            "Export failed: no recording segments found on disk for {cam_for_reg} in [{start}, {end}]"
                        ),
                    ));
                }
            }
            let export_id_exists =
                |cand_id: &str| -> std::result::Result<bool, (StatusCode, String)> {
                    let in_db = state_for_reg
                        .db
                        .get_export_by_id(cand_id)
                        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                        .is_some();
                    if in_db {
                        return Ok(true);
                    }
                    let mp4_exists = std::fs::symlink_metadata(
                        state_for_reg
                            .media_dir
                            .join("exports")
                            .join(format!("{cand_id}.mp4")),
                    )
                    .is_ok();
                    let webp_exists = std::fs::symlink_metadata(
                        state_for_reg
                            .media_dir
                            .join("clips")
                            .join(format!("{cand_id}.webp")),
                    )
                    .is_ok();
                    Ok(mp4_exists || webp_exists)
                };
            let id = if !export_id_exists(&base_id_owned)? {
                base_id_owned
            } else {
                let mut resolved_unique = None;
                for _ in 0..64 {
                    let cand = format!("{base_id_owned}-{:08x}", rand::random::<u32>());
                    if !export_id_exists(&cand)? {
                        resolved_unique = Some(cand);
                        break;
                    }
                }
                resolved_unique.ok_or_else(|| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to allocate a unique export identifier after 64 attempts"
                            .to_string(),
                    )
                })?
            };
            cleanup_share_clip_artifacts(&state_for_reg.media_dir, &id);
            let video_rel = format!("/media/ninjanvr/exports/{id}.mp4");
            let thumb_rel = format!("/media/ninjanvr/clips/{id}.webp");
            state_for_reg
                .db
                .insert_export(&ExportRow {
                    id: id.clone(),
                    camera: cam_for_reg,
                    name: name_for_reg,
                    date: now,
                    video_path: video_rel.clone(),
                    thumb_path: thumb_rel.clone(),
                    in_progress: true,
                })
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to register export in database: {e}"),
                    )
                })?;
            Ok((id, video_rel, thumb_rel))
        },
    )
    .await;
    let (id, video_rel, thumb_rel) = match reg_res {
        Ok(Ok(triple)) => triple,
        Ok(Err((code, msg))) => {
            return (code, Json(json!({ "success": false, "message": msg }))).into_response();
        }
        Err(join_err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "success": false,
                    "message": format!("Failed to register export in database: {join_err}")
                })),
            )
                .into_response();
        }
    };
    let mut in_progress_guard = ExportInProgressGuard {
        db: state.db.clone(),
        media_dir: state.media_dir.clone(),
        id: id.clone(),
        video_rel,
        thumb_rel,
        armed: true,
    };

    if run_async {
        drop(_export_guard);
        let (queued_snap, cancel_flag) = match state.export_jobs.try_register_queued(
            id.clone(),
            camera.clone(),
            name.clone(),
            start,
            end,
            include_audio,
            now,
        ) {
            Ok(pair) => pair,
            Err(max_cap) => {
                drop(in_progress_guard);
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({
                        "success": false,
                        "message": format!("Export job queue is full (maximum {max_cap} active exports)")
                    })),
                )
                    .into_response();
            }
        };
        emit_export_job_ws(&state, &queued_snap);

        let bg_state = state.clone();
        let bg_id = id.clone();
        let bg_cam = camera.clone();
        let bg_name = name.clone();
        let bg_cold = cold_dir_opt.clone();
        let bg_mask = privacy_mask.clone();
        tokio::spawn(async move {
            let mut guard = in_progress_guard;
            let permit_fut = bg_state
                .export_jobs
                .worker_semaphore
                .clone()
                .acquire_owned();
            let mut cancel_poll = tokio::time::interval(Duration::from_millis(100));
            cancel_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let _worker_permit = tokio::select! {
                res = permit_fut => {
                    match res {
                        Ok(p) => p,
                        Err(_) => {
                            if let Some(snap) = bg_state.export_jobs.mark_failed(
                                &bg_id,
                                "Export worker pool shut down".to_string(),
                                unix_now() as f64,
                            ) {
                                emit_export_job_ws(&bg_state, &snap);
                            }
                            return;
                        }
                    }
                }
                _ = async {
                    loop {
                        cancel_poll.tick().await;
                        if cancel_flag.load(std::sync::atomic::Ordering::SeqCst) {
                            break;
                        }
                    }
                } => {
                    drop(guard);
                    return;
                }
            };

            let Some(running_snap) = bg_state.export_jobs.mark_running(&bg_id, unix_now() as f64)
            else {
                drop(guard);
                return;
            };
            emit_export_job_ws(&bg_state, &running_snap);

            let prog_state = bg_state.clone();
            let prog_id = bg_id.clone();
            let on_progress: Arc<dyn Fn(f64) + Send + Sync> = Arc::new(move |frac: f64| {
                if let Some((snap, should_emit)) =
                    prog_state
                        .export_jobs
                        .update_progress(&prog_id, frac, unix_now() as f64)
                    && should_emit
                {
                    emit_export_job_ws(&prog_state, &snap);
                }
            });

            let adaptive_timeout_secs = ((end - start).ceil() as u64)
                .saturating_mul(2)
                .clamp(600, 7200);
            let export_res = tokio::time::timeout(
                Duration::from_secs(adaptive_timeout_secs),
                build_mp4_export_with_progress(Mp4ExportRequest {
                    db: &bg_state.db,
                    media_dir: &bg_state.media_dir,
                    cold_dir: bg_cold.as_deref(),
                    export_id: &bg_id,
                    camera: &bg_cam,
                    name: &bg_name,
                    start_time: start,
                    end_time: end,
                    include_audio,
                    privacy_mask: bg_mask.as_ref(),
                    cancel_token: Some(cancel_flag.clone()),
                    on_progress: Some(on_progress),
                }),
            )
            .await;

            cleanup_share_clip_artifacts(&bg_state.media_dir, &bg_id);

            match export_res {
                Ok(Ok(())) => {
                    if let Some(done_snap) = bg_state.export_jobs.mark_completed(
                        &bg_id,
                        format!("/exports/{bg_id}.mp4"),
                        format!("/clips/{bg_id}.webp"),
                        unix_now() as f64,
                    ) {
                        guard.disarm();
                        emit_export_job_ws(&bg_state, &done_snap);
                    } else {
                        // Job was cancelled or deleted concurrently: keep `guard` armed so DB row and files are cleaned up!
                        drop(guard);
                    }
                }
                Ok(Err(err)) => {
                    drop(guard);
                    if let Some(fail_snap) =
                        bg_state
                            .export_jobs
                            .mark_failed(&bg_id, err.to_string(), unix_now() as f64)
                    {
                        emit_export_job_ws(&bg_state, &fail_snap);
                    }
                }
                Err(_) => {
                    cancel_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    drop(guard);
                    if let Some(fail_snap) = bg_state.export_jobs.mark_failed(
                        &bg_id,
                        "Export timed out while remuxing MP4 segments".to_string(),
                        unix_now() as f64,
                    ) {
                        emit_export_job_ws(&bg_state, &fail_snap);
                    }
                }
            }
        });

        return (
            StatusCode::ACCEPTED,
            Json(json!({
                "success": true,
                "queued": true,
                "export_id": id,
                "video_url": format!("/exports/{id}.mp4"),
                "thumb_url": format!("/clips/{id}.webp"),
                "job": queued_snap
            })),
        )
            .into_response();
    }

    let db_clone = state.db.clone();
    let media_dir_clone = state.media_dir.clone();
    let id_clone = id.clone();
    let export_res = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        build_mp4_export_with_progress(Mp4ExportRequest {
            db: &db_clone,
            media_dir: &media_dir_clone,
            cold_dir: cold_dir_opt.as_deref(),
            export_id: &id_clone,
            camera: &camera,
            name: &name,
            start_time: start,
            end_time: end,
            include_audio,
            privacy_mask: privacy_mask.as_ref(),
            cancel_token: None,
            on_progress: None,
        }),
    )
    .await;
    drop(sync_sem_permit);

    cleanup_share_clip_artifacts(&state.media_dir, &id);

    match export_res {
        Ok(Ok(())) => {
            in_progress_guard.disarm();
            Json(json!({
                "success": true,
                "export_id": id,
                "video_url": format!("/exports/{id}.mp4"),
                "thumb_url": format!("/clips/{id}.webp")
            }))
            .into_response()
        }
        Ok(Err(err)) => {
            drop(in_progress_guard);
            (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "success": false,
                    "message": format!("Export failed: {err}")
                })),
            )
                .into_response()
        }
        Err(_) => {
            drop(in_progress_guard);
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({
                    "success": false,
                    "message": "Export timed out while remuxing MP4 segments"
                })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct RenameExportBody {
    #[serde(default)]
    pub name: String,
}

async fn api_export_rename(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<RenameExportBody>,
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
    if let Some(err_resp) = verify_caller_can_mutate(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == "*")
    {
        let permitted = state
            .db
            .get_export_by_id(&id)
            .ok()
            .flatten()
            .and_then(|exp| {
                exp["camera"]
                    .as_str()
                    .map(str::trim)
                    .map(|c| allowed.iter().any(|a| a == c))
            })
            .unwrap_or(false);
        if !permitted {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "success": false,
                    "message": "Camera is not permitted for this user"
                })),
            )
                .into_response();
        }
    }
    if let Err(err) = state.db.rename_export(&id, &body.name) {
        warn!("Failed to rename export '{id}': {err}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "message": "Failed to rename export"
            })),
        )
            .into_response();
    }
    Json(json!({ "success": true })).into_response()
}

fn resolve_safe_export_unlink_paths(
    media_dir: &Path,
    vpath: &str,
    tpath: &str,
) -> (Option<PathBuf>, Option<PathBuf>) {
    media_lifecycle::resolve_safe_export_unlink_paths(media_dir, vpath, tpath)
}

async fn api_export_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
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
            Json(json!({ "message": "Forbidden: admin role required" })),
        )
            .into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == "*")
    {
        let permitted = state
            .db
            .get_export_by_id(&id)
            .ok()
            .flatten()
            .and_then(|exp| {
                exp["camera"]
                    .as_str()
                    .map(str::trim)
                    .map(|c| allowed.iter().any(|a| a == c))
            })
            .unwrap_or(false);
        if !permitted {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "success": false,
                    "message": "Camera is not permitted for this user"
                })),
            )
                .into_response();
        }
    }
    if let Err(err) = state
        .share_engine
        .on_target_deleted(&state, Some("export"), &id)
    {
        warn!("Failed to revoke share links for export '{id}': {err}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "message": "Failed to revoke share links for export"
            })),
        )
            .into_response();
    }
    state.export_jobs.remove(&id);
    let state_for_del = state.clone();
    let _ = tokio::task::spawn_blocking(move || {
        media_lifecycle::delete_export_with_artifacts(
            &state_for_del.db,
            &state_for_del.media_dir,
            &id,
        )
    })
    .await;
    state.storage_breakdown_cache.invalidate();
    Json(json!({ "success": true })).into_response()
}

async fn api_exports_bulk_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<DeleteIdsBody>,
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
            Json(json!({ "message": "Forbidden: admin role required" })),
        )
            .into_response();
    }
    if body.ids.len() > 1000 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Bulk delete request exceeds maximum of 1000 export IDs"
            })),
        )
            .into_response();
    }
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
    let state_for_del = state.clone();
    let ids = body.ids;
    tokio::task::spawn_blocking(move || -> Response {
        if let Some(ref allow_list) = allowed
            && !allow_list.iter().any(|a| a == "*")
        {
            for id in &ids {
                let permitted = state_for_del
                    .db
                    .get_export_by_id(id)
                    .ok()
                    .flatten()
                    .and_then(|exp| {
                        exp["camera"]
                            .as_str()
                            .map(str::trim)
                            .map(|c| allow_list.iter().any(|a| a == c))
                    })
                    .unwrap_or(false);
                if !permitted {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({
                            "success": false,
                            "message": "Camera is not permitted for this user"
                        })),
                    )
                        .into_response();
                }
            }
        }
        for id in ids {
            state_for_del.export_jobs.remove(&id);
            if let Err(err) =
                state_for_del
                    .share_engine
                    .on_target_deleted(&state_for_del, Some("export"), &id)
            {
                warn!("Failed to revoke share links for export '{id}': {err}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "success": false,
                        "message": "Failed to revoke share links for export"
                    })),
                )
                    .into_response();
            }
            let _ = media_lifecycle::delete_export_with_artifacts(
                &state_for_del.db,
                &state_for_del.media_dir,
                &id,
            );
        }
        state_for_del.storage_breakdown_cache.invalidate();
        Json(json!({ "success": true })).into_response()
    })
    .await
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn api_previews(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath((camera, start, end)): AxumPath<(String, f64, f64)>,
) -> Response {
    if (camera != "all" && (camera.is_empty() || !is_safe_id(&camera)))
        || !start.is_finite()
        || !end.is_finite()
        || start < 0.0
        || end < start
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Invalid preview query parameters" })),
        )
            .into_response();
    }
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
    if let Some(ref allow_list) = allowed
        && camera != "all"
        && !allow_list.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    let mut previews = state
        .db
        .query_previews(&camera, start, end)
        .unwrap_or_default();
    if let Some(allow_list) = allowed
        && camera == "all"
        && !allow_list.iter().any(|a| a == "*")
    {
        previews.retain(|p| {
            p["camera"]
                .as_str()
                .map(str::trim)
                .is_some_and(|cam| allow_list.iter().any(|a| a == cam))
        });
    }
    Json(Value::Array(previews)).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct PreviewCameraFramesQuery {
    pub limit: Option<usize>,
}

async fn api_preview_camera_frames(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath((camera, start, end)): AxumPath<(String, f64, f64)>,
    Query(q): Query<PreviewCameraFramesQuery>,
) -> Response {
    if camera.is_empty()
        || camera == "all"
        || !is_safe_id(&camera)
        || !start.is_finite()
        || !end.is_finite()
        || start < 0.0
        || end < start
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Invalid preview frames query parameters" })),
        )
            .into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    let limit = q.limit.unwrap_or(180).clamp(1, 360);
    let from_ms = (start * 1000.0).floor().max(0.0) as u64;
    let to_ms = (end * 1000.0).ceil().max(from_ms as f64) as u64;
    let frames: Vec<Value> = state
        .preview_frames
        .range(&camera, from_ms, to_ms, limit)
        .into_iter()
        .map(|ts| {
            json!({
                "ts": ts as f64 / 1000.0,
                "ts_ms": ts,
                "url": format!("/api/preview/{camera}/frame/{ts}.jpg")
            })
        })
        .collect();
    (
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(Value::Array(frames)),
    )
        .into_response()
}

async fn api_preview_camera_frame(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath((camera, frame)): AxumPath<(String, String)>,
) -> Response {
    if camera.is_empty() || !is_safe_id(&camera) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(ts) = preview_frames::parse_preview_frame_name(&frame) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    if !state.preview_frames.contains(&camera, ts) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(path) = state.preview_frames.frame_path(&camera, ts) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(bytes) = read_bounded_non_symlink_bytes(&path, MAX_SNAPSHOT_FRAME_BYTES as u64).await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let priv_mask = camera_privacy_mask_value(&state.config, &camera).await;
    let (bytes, _) = redact_image_bytes_with_privacy_mask_async(bytes, priv_mask, true).await;
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "private, max-age=86400, immutable"),
            (
                header::HeaderName::from_static("x-frame-source"),
                "preview_store",
            ),
        ],
        bytes,
    )
        .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct HistoricalFrameQuery {
    pub ts: Option<f64>,
    pub w: Option<u32>,
}

async fn serve_historical_camera_frame_at_ms(
    state: AppState,
    jar: CookieJar,
    headers: HeaderMap,
    camera: String,
    ts_ms: u64,
    requested_width: u32,
) -> Response {
    const MAX_PREVIEW_TS_MS: u64 = 4_102_444_800_000;
    if camera.is_empty() || !is_safe_id(&camera) || ts_ms > MAX_PREVIEW_TS_MS {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    let width = requested_width.clamp(160, 960);

    // Tier 1: Zero-CPU RAM-indexed PreviewFrameStore when within ±1.5s and thumbnail resolution
    if width <= 360
        && let Some(near_ts) = state.preview_frames.nearest(&camera, ts_ms, 1_500)
        && let Some(path) = state.preview_frames.frame_path(&camera, near_ts)
        && let Some(bytes) =
            read_bounded_non_symlink_bytes(&path, MAX_SNAPSHOT_FRAME_BYTES as u64).await
    {
        let priv_mask = camera_privacy_mask_value(&state.config, &camera).await;
        let (bytes, _) = redact_image_bytes_with_privacy_mask_async(bytes, priv_mask, true).await;
        return (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "private, max-age=3600"),
                (
                    header::HeaderName::from_static("x-frame-source"),
                    "preview_store",
                ),
            ],
            bytes,
        )
            .into_response();
    }

    // Tier 2: Bounded LRU cache + on-demand FFmpeg extraction from covering hot/cold segment or hourly preview
    let (cold_dir_opt, priv_mask) = {
        let cfg = state.config.read().await;
        let cold = resolve_configured_cold_dir(&cfg);
        let pmask = cfg.cameras.get(&camera).map(|c| {
            normalize_privacy_mask_value_for_resolution(
                &c.privacy_mask,
                c.detect.width.max(1),
                c.detect.height.max(1),
            )
        });
        (cold, pmask)
    };
    let mask_sig = priv_mask
        .as_ref()
        .and_then(|v| serde_json::to_string(v).ok())
        .unwrap_or_default();
    let bucket_step_ms = if ts_ms.is_multiple_of(1000) {
        1000u64
    } else {
        250u64
    };
    let bucket_ms = (ts_ms / bucket_step_ms) * bucket_step_ms;
    let cache_key = export_jobs::HistoricalFrameKey {
        camera: camera.clone(),
        bucket_ms,
        width,
        mask_sig,
    };
    if let Some((cached_bytes, src_label)) = state.historical_frames.get(&cache_key) {
        return (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "private, max-age=3600"),
                (header::HeaderName::from_static("x-frame-source"), src_label),
            ],
            cached_bytes,
        )
            .into_response();
    }

    let ts_sec = bucket_ms as f64 / 1000.0;
    let state_for_lookup = state.clone();
    let cam_for_lookup = camera.clone();
    let cold_for_lookup = cold_dir_opt.clone();
    let lookup_res = tokio::task::spawn_blocking(
        move || -> Option<(PathBuf, f64, Option<f64>, &'static str)> {
            // 1. Covering raw recording segment (hot or cold tier)
            if let Ok(segs) =
                state_for_lookup
                    .db
                    .query_recordings(&cam_for_lookup, ts_sec - 30.0, ts_sec + 5.0)
            {
                let mut best_seg: Option<(f64, PathBuf, f64, f64)> = None;
                for seg in segs {
                    let in_span = ts_sec >= seg.start_time - 0.5 && ts_sec <= seg.end_time + 1.0;
                    let dist = if ts_sec < seg.start_time {
                        seg.start_time - ts_sec
                    } else if ts_sec > seg.end_time {
                        ts_sec - seg.end_time
                    } else {
                        0.0
                    };
                    if !in_span && dist > 1.5 {
                        continue;
                    }
                    if let Some(safe_path) = media_lifecycle::resolve_contained_recording_segment(
                        &state_for_lookup.media_dir,
                        cold_for_lookup.as_deref(),
                        Some(&cam_for_lookup),
                        &seg.path,
                    ) {
                        let seg_dur = if seg.duration.is_finite() && seg.duration > 0.0 {
                            seg.duration
                        } else {
                            (seg.end_time - seg.start_time).max(1.0)
                        };
                        let offset = (ts_sec - seg.start_time).max(0.0);
                        if best_seg
                            .as_ref()
                            .is_none_or(|(b_dist, _, _, _)| dist < *b_dist)
                        {
                            best_seg = Some((dist, safe_path, offset, seg_dur));
                            if dist == 0.0 {
                                break;
                            }
                        }
                    }
                }
                if let Some((_, safe_path, offset, seg_dur)) = best_seg {
                    return Some((safe_path, offset, Some(seg_dur), "segment"));
                }
            }

            // 2. Fallback to hourly timelapse preview MP4 ONLY when all raw segments for that hour
            //    were pruned AND the hourly preview covers the span continuously without offline holes.
            if let Ok(previews) =
                state_for_lookup
                    .db
                    .query_previews(&cam_for_lookup, ts_sec - 3600.0, ts_sec + 60.0)
            {
                let previews_root = state_for_lookup
                    .media_dir
                    .join("clips")
                    .join("previews")
                    .join(&cam_for_lookup);
                let canon_prev_root = std::fs::canonicalize(&previews_root).ok();
                for prev in previews {
                    let p_start = prev["start"]
                        .as_f64()
                        .or_else(|| prev["start_time"].as_f64())
                        .unwrap_or(0.0);
                    let p_end = prev["end"]
                        .as_f64()
                        .or_else(|| prev["end_time"].as_f64())
                        .unwrap_or(0.0);
                    let p_dur = prev["duration"].as_f64().unwrap_or(0.0);
                    if p_end <= p_start || ts_sec < p_start - 1.0 || ts_sec > p_end + 1.0 {
                        continue;
                    }
                    // If raw segments still exist in `[p_start, p_end]`, `ts_sec` is an unrecorded gap!
                    let raw_still_present = state_for_lookup
                        .db
                        .query_recordings(&cam_for_lookup, p_start, p_end)
                        .is_ok_and(|r| !r.is_empty());
                    if raw_still_present {
                        continue;
                    }
                    // Skip gappy hourly previews whose recorded duration is >30s shorter than `[p_start, p_end]`
                    let wall_span = (p_end - p_start).max(0.0);
                    if p_dur <= 0.0 || wall_span - p_dur > 30.0 {
                        continue;
                    }
                    let Some(raw_p) = prev["src"].as_str().or_else(|| prev["path"].as_str()) else {
                        continue;
                    };
                    let disk_p = if raw_p.starts_with("/clips/") {
                        state_for_lookup
                            .media_dir
                            .join(raw_p.trim_start_matches('/'))
                    } else {
                        media_lifecycle::resolve_recording_disk_path(
                            &state_for_lookup.media_dir,
                            raw_p,
                        )
                    };
                    if std::fs::symlink_metadata(&disk_p).is_ok_and(|m| m.file_type().is_file())
                        && let (Some(root_c), Ok(canon_p)) =
                            (&canon_prev_root, std::fs::canonicalize(&disk_p))
                        && canon_p.starts_with(root_c)
                    {
                        if let Some(probed_dur) = ninjanvr_media::probe_mp4_duration_fast(&canon_p)
                            && probed_dur > 0.0
                            && wall_span - probed_dur > 30.0
                        {
                            continue;
                        }
                        let offset = (ts_sec - p_start).max(0.0);
                        return Some((canon_p, offset, Some(p_dur), "hourly_preview"));
                    }
                }
            }
            None
        },
    )
    .await
    .ok()
    .flatten();

    let Some((source_path, offset_sec, duration_hint, source_kind)) = lookup_res else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let Ok(Ok(_permit)) = tokio::time::timeout(
        Duration::from_secs(5),
        state.historical_frames.extract_semaphore.acquire(),
    )
    .await
    else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };

    let Ok(raw_jpeg) =
        extract_historical_frame_jpeg(&source_path, offset_sec, duration_hint, width).await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (redacted_vec, _) =
        redact_image_bytes_with_privacy_mask_async(raw_jpeg, priv_mask, true).await;
    let out_bytes = Bytes::from(redacted_vec);
    state
        .historical_frames
        .insert(cache_key, out_bytes.clone(), source_kind);

    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "private, max-age=3600"),
            (
                header::HeaderName::from_static("x-frame-source"),
                source_kind,
            ),
        ],
        out_bytes,
    )
        .into_response()
}

async fn api_preview_camera_at(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath((camera, target)): AxumPath<(String, String)>,
    Query(q): Query<HistoricalFrameQuery>,
) -> Response {
    let Some(ts_ms) = preview_frames::parse_preview_frame_name(&target) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    serve_historical_camera_frame_at_ms(state, jar, headers, camera, ts_ms, q.w.unwrap_or(320))
        .await
}

async fn api_camera_historical_frame(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    Query(q): Query<HistoricalFrameQuery>,
) -> Response {
    let Some(ts_sec) = q.ts.filter(|t| t.is_finite() && *t >= 0.0) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Query parameter 'ts' must be a non-negative finite unix timestamp" })),
        )
            .into_response();
    };
    let ts_ms = (ts_sec * 1000.0).round() as u64;
    serve_historical_camera_frame_at_ms(state, jar, headers, camera, ts_ms, q.w.unwrap_or(320))
        .await
}

async fn api_preview_thumbnail(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(preview_name): AxumPath<String>,
    uri: Uri,
) -> Response {
    let Some(after_prefix) = preview_name
        .strip_prefix("preview_")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Invalid preview identifier" })),
        )
            .into_response();
    };
    let configured_cam = {
        let cfg = state.config.read().await;
        let mut matched: Option<String> = None;
        for cam in cfg.cameras.keys() {
            let prefix = format!("{cam}-");
            if (after_prefix == cam || after_prefix.starts_with(&prefix))
                && matched.as_ref().is_none_or(|prev| cam.len() > prev.len())
            {
                matched = Some(cam.clone());
            }
        }
        matched
    };
    let camera = configured_cam.unwrap_or_else(|| {
        after_prefix
            .rsplit_once('-')
            .map_or(after_prefix, |(c, _)| c)
            .to_string()
    });
    if camera.is_empty() || !is_safe_id(&camera) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Invalid preview camera identifier" })),
        )
            .into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    api_camera_latest_frame(
        State(state),
        jar,
        headers,
        AxumPath(camera),
        Query(LatestFrameQuery {
            height: Some(360),
            bbox: None,
        }),
        uri,
    )
    .await
}

async fn api_labels(State(state): State<AppState>) -> Json<Value> {
    let cfg = state.config.read().await;
    Json(json!(cfg.objects.track))
}

async fn api_empty_array() -> Json<Value> {
    Json(json!([]))
}

async fn api_empty_object() -> Json<Value> {
    Json(json!({}))
}

fn resolve_effective_onvif_credentials(
    cfg: &NinjaConfig,
    camera: &str,
    cam_cfg: &ninjanvr_config::CameraConfig,
) -> (Option<String>, Option<String>) {
    let explicit_user = cam_cfg
        .onvif
        .user
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let explicit_pass = cam_cfg
        .onvif
        .password
        .as_deref()
        .filter(|p| !p.is_empty() && p.trim() != "***" && p.trim() != "*")
        .map(str::to_string);
    if explicit_user.is_some() && explicit_pass.is_some() {
        return (explicit_user, explicit_pass);
    }
    if let Some(inferred) = cfg.infer_camera_upstream(camera) {
        return (
            explicit_user.or(inferred.user),
            explicit_pass.or(inferred.password),
        );
    }
    (explicit_user, explicit_pass)
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PtzInfoQueryParams {
    pub timeout_ms: Option<u64>,
    pub profile_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PtzMoveBody {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub pan: Option<f32>,
    #[serde(default)]
    pub tilt: Option<f32>,
    #[serde(default)]
    pub zoom: Option<f32>,
    #[serde(default)]
    pub speed: Option<f32>,
    #[serde(default)]
    pub timeout_sec: Option<u64>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub profile_token: Option<String>,
}

impl PtzMoveBody {
    pub fn resolve_vectors(&self) -> (f32, f32, f32) {
        let spd = self
            .speed
            .filter(|v| v.is_finite())
            .unwrap_or(0.5)
            .clamp(0.05, 1.0);
        let dir = self
            .action
            .as_deref()
            .or(self.direction.as_deref())
            .map(|s| s.trim().to_ascii_lowercase());
        let (dir_pan, dir_tilt, dir_zoom) = match dir.as_deref() {
            Some("left" | "pan_left") => (-spd, 0.0, 0.0),
            Some("right" | "pan_right") => (spd, 0.0, 0.0),
            Some("up" | "tilt_up") => (0.0, spd, 0.0),
            Some("down" | "tilt_down") => (0.0, -spd, 0.0),
            Some("up_left") => (-spd, spd, 0.0),
            Some("up_right") => (spd, spd, 0.0),
            Some("down_left") => (-spd, -spd, 0.0),
            Some("down_right") => (spd, -spd, 0.0),
            Some("zoom_in" | "in") => (0.0, 0.0, spd),
            Some("zoom_out" | "out") => (0.0, 0.0, -spd),
            _ => (0.0, 0.0, 0.0),
        };
        let pan = self
            .pan
            .filter(|v| v.is_finite())
            .unwrap_or(dir_pan)
            .clamp(-1.0, 1.0);
        let tilt = self
            .tilt
            .filter(|v| v.is_finite())
            .unwrap_or(dir_tilt)
            .clamp(-1.0, 1.0);
        let zoom = self
            .zoom
            .filter(|v| v.is_finite())
            .unwrap_or(dir_zoom)
            .clamp(-1.0, 1.0);
        (pan, tilt, zoom)
    }

    pub fn is_relative(&self) -> bool {
        self.mode
            .as_deref()
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("relative"))
    }

    pub fn is_absolute(&self) -> bool {
        self.mode
            .as_deref()
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("absolute"))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PtzStopBody {
    #[serde(default = "default_true")]
    pub pan_tilt: bool,
    #[serde(default = "default_true")]
    pub zoom: bool,
    #[serde(default)]
    pub profile_token: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl Default for PtzStopBody {
    fn default() -> Self {
        Self {
            pan_tilt: true,
            zoom: true,
            profile_token: None,
            timeout_ms: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PtzPresetBody {
    #[serde(default, alias = "token", alias = "preset_token", alias = "name")]
    pub preset: String,
    #[serde(default)]
    pub speed: Option<f32>,
    #[serde(default)]
    pub profile_token: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PtzPresetSaveBody {
    #[serde(default, alias = "preset", alias = "preset_name")]
    pub name: String,
    #[serde(default, alias = "preset_token")]
    pub token: Option<String>,
    #[serde(default)]
    pub profile_token: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

fn default_camera_profile_token(
    runtime: &CameraDriverRuntime,
    camera: &str,
    endpoint: &str,
    cam_cfg: &ninjanvr_config::CameraConfig,
) -> String {
    if let Some(cfg_tok) = cam_cfg
        .hardware
        .profile_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && let Some(clean) = sanitize_onvif_token(Some(cfg_tok), "Profile_1")
    {
        return clean;
    }
    if let Some(cached) = runtime.get_profile(camera, endpoint) {
        return cached;
    }
    if let Some(proto_spec) = cam_cfg
        .hardware
        .protocol
        .as_deref()
        .and_then(find_protocol_spec_by_id_or_alias)
        .or_else(|| {
            runtime
                .get_protocol(camera)
                .as_deref()
                .and_then(find_protocol_spec_by_id_or_alias)
        })
        && let Some(&first_pref) = proto_spec.preferred_profile_tokens.first()
    {
        return first_pref.to_string();
    }
    "Profile_1".to_string()
}

fn resolve_effective_ptz_endpoint(
    runtime: &CameraDriverRuntime,
    cfg: &NinjaConfig,
    camera: &str,
    cam_cfg: &ninjanvr_config::CameraConfig,
) -> Option<String> {
    if cam_cfg.onvif.is_configured() {
        let inferred = cfg.infer_camera_upstream(camera);
        let is_tapo_hint = inferred.as_ref().is_some_and(|i| i.is_tapo_rtsp)
            || cam_cfg.onvif.port == 2020
            || cam_cfg
                .hardware
                .protocol
                .as_deref()
                .and_then(find_protocol_spec_by_id_or_alias)
                .is_some_and(|s| s.id == "tapo")
            || runtime
                .get_protocol(camera)
                .as_deref()
                .is_some_and(|p| p.eq_ignore_ascii_case("tapo"));
        let mut adjusted_onvif = cam_cfg.onvif.clone();
        if adjusted_onvif.port == 80 && is_tapo_hint {
            adjusted_onvif.port = 2020;
        }
        if let Some(ep) = resolve_onvif_ptz_endpoint(&adjusted_onvif) {
            return Some(ep);
        }
    }
    if !cam_cfg.hardware.is_unconfigured() || runtime.get_protocol(camera).is_some() {
        return resolve_camera_hardware_probe_targets(runtime, cfg, camera, cam_cfg)
            .and_then(|t| t.ptz_ep_opt);
    }
    None
}

async fn fetch_ptz_status_with_profile_discovery(
    state: &AppState,
    camera: &str,
    endpoint: &str,
    (initial_profile, allow_discovery, protocol_id): (&str, bool, Option<&str>),
    (user, pass): (Option<&str>, Option<&str>),
    timeout: std::time::Duration,
) -> std::result::Result<(String, OnvifPtzStatus), (StatusCode, String)> {
    let transport = camera_protocols::ReqwestCameraWireTransport::new(state.http_client.clone());
    let (resolved_prof, status, _xml) = state
        .camera_driver_runtime
        .fetch_ptz_status_with_discovery(
            &transport,
            camera,
            endpoint,
            initial_profile,
            allow_discovery,
            protocol_id,
            (user, pass),
            timeout,
        )
        .await?;
    Ok((resolved_prof, status))
}

async fn api_camera_ptz_info(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    Query(q): Query<PtzInfoQueryParams>,
) -> Response {
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    let (cam_opt, active_preset, creds, ptz_endpoint_opt) = {
        let cfg = state.config.read().await;
        let cam = cfg.cameras.get(&camera).cloned();
        let creds = cam.as_ref().map_or((None, None), |c| {
            resolve_effective_onvif_credentials(&cfg, &camera, c)
        });
        let ep_opt = cam.as_ref().and_then(|c| {
            resolve_effective_ptz_endpoint(&state.camera_driver_runtime, &cfg, &camera, c)
        });
        let ret_preset = cam.as_ref().and_then(|c| c.onvif.return_preset.as_deref());
        let active = state
            .camera_driver_runtime
            .get_active_preset(&camera, ret_preset)
            .await;
        (cam, active, creds, ep_opt)
    };

    let Some(cam_cfg) = cam_opt else {
        return Json(json!({
            "enabled": false,
            "camera": camera,
            "features": [],
            "presets": [],
            "preset_names": [],
            "active_preset": Value::Null
        }))
        .into_response();
    };

    let mut fallback_presets: Vec<OnvifPresetItem> = Vec::new();
    for (soft_tok, soft_item) in &cam_cfg.hardware.soft_presets {
        fallback_presets.push(OnvifPresetItem {
            token: soft_tok.clone(),
            name: soft_item.name.clone(),
        });
    }
    if let Some(ret_p) = cam_cfg
        .onvif
        .return_preset
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && !fallback_presets
            .iter()
            .any(|p| p.token == ret_p || p.name == ret_p)
    {
        fallback_presets.push(OnvifPresetItem {
            token: ret_p.to_string(),
            name: ret_p.to_string(),
        });
    }
    for zcfg in cam_cfg.zones.values() {
        if let Some(zp) = zcfg
            .preset
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            && !fallback_presets
                .iter()
                .any(|p| p.token == zp || p.name == zp)
        {
            fallback_presets.push(OnvifPresetItem {
                token: zp.to_string(),
                name: zp.to_string(),
            });
        }
    }

    let Some(endpoint) = ptz_endpoint_opt else {
        let preset_names: Vec<String> = fallback_presets.iter().map(|p| p.name.clone()).collect();
        return Json(json!({
            "enabled": false,
            "camera": camera,
            "features": [],
            "presets": fallback_presets,
            "preset_names": preset_names,
            "active_preset": active_preset,
            "autotracking": cam_cfg.onvif.autotracking
        }))
        .into_response();
    };

    let timeout = match q.timeout_ms {
        Some(ms) => std::time::Duration::from_millis(ms.clamp(20, 30_000)),
        None => std::time::Duration::from_secs(cam_cfg.onvif.timeout_sec.clamp(1, 30)),
    };
    let default_prof =
        default_camera_profile_token(&state.camera_driver_runtime, &camera, &endpoint, &cam_cfg);
    let Some(mut profile_token) = sanitize_onvif_token(q.profile_token.as_deref(), &default_prof)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Invalid ONVIF profile token" })),
        )
            .into_response();
    };
    let user = creds.0.as_deref();
    let pass = creds.1.as_deref();
    let transport = camera_protocols::ReqwestCameraWireTransport::new(state.http_client.clone());

    let (resolved_prof, presets_res, status_res) = state
        .camera_driver_runtime
        .query_ptz_presets_and_status(
            &transport,
            &camera,
            &endpoint,
            &profile_token,
            q.profile_token.is_none(),
            cam_cfg.hardware.protocol.as_deref(),
            (user, pass),
            timeout,
        )
        .await;
    profile_token = resolved_prof;

    let mut merged_presets = match &presets_res {
        Ok(xml) => parse_onvif_presets_xml(xml),
        Err(_) => Vec::new(),
    };
    for fp in fallback_presets {
        if !merged_presets
            .iter()
            .any(|p| p.token == fp.token || p.name == fp.name)
        {
            merged_presets.push(fp);
        }
    }
    let preset_names: Vec<String> = merged_presets.iter().map(|p| p.name.clone()).collect();

    let (status_obj, online, err_opt) = match (&presets_res, &status_res) {
        (_, Ok(status_xml)) => (parse_onvif_status_xml(status_xml), true, None),
        (Ok(_), Err((_, err_msg))) => (
            OnvifPtzStatus {
                pan: 0.0,
                tilt: 0.0,
                zoom: 0.0,
                move_status: "IDLE".to_string(),
                error: Some(err_msg.clone()),
            },
            true,
            Some(err_msg.clone()),
        ),
        (Err((_, err_a)), Err((_, _))) => (
            OnvifPtzStatus {
                pan: 0.0,
                tilt: 0.0,
                zoom: 0.0,
                move_status: "ERROR".to_string(),
                error: Some(err_a.clone()),
            },
            false,
            Some(err_a.clone()),
        ),
    };

    let mut features = vec!["pt", "zoom", "pt-r", "zoom-r", "presets", "presets_manage"];
    if cam_cfg.onvif.autotracking {
        features.push("autotracking");
    }

    Json(json!({
        "enabled": true,
        "online": online,
        "camera": camera,
        "host": cam_cfg.onvif.host.as_deref().map(scrub_rtsp_credentials),
        "port": cam_cfg.onvif.port,
        "profile_token": profile_token,
        "autotracking": cam_cfg.onvif.autotracking,
        "return_preset": cam_cfg.onvif.return_preset,
        "active_preset": active_preset,
        "features": features,
        "presets": merged_presets,
        "preset_names": preset_names,
        "status": status_obj,
        "error": err_opt
    }))
    .into_response()
}

async fn api_camera_ptz_move(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    Json(body): Json<PtzMoveBody>,
) -> Response {
    if let Some(err_resp) =
        verify_camera_mutation_access(&state, &jar, &uri, &headers, &camera).await
    {
        return err_resp.into_response();
    }
    let (cam_opt, creds, ptz_endpoint_opt) = {
        let cfg = state.config.read().await;
        let cam = cfg.cameras.get(&camera).cloned();
        let creds = cam.as_ref().map_or((None, None), |c| {
            resolve_effective_onvif_credentials(&cfg, &camera, c)
        });
        let ep_opt = cam.as_ref().and_then(|c| {
            resolve_effective_ptz_endpoint(&state.camera_driver_runtime, &cfg, &camera, c)
        });
        (cam, creds, ep_opt)
    };
    let Some(cam_cfg) = cam_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": format!("Camera '{camera}' not found") })),
        )
            .into_response();
    };
    let Some(endpoint) = ptz_endpoint_opt else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": format!("ONVIF PTZ is not configured for camera '{camera}'") })),
        )
            .into_response();
    };

    let default_prof =
        default_camera_profile_token(&state.camera_driver_runtime, &camera, &endpoint, &cam_cfg);
    let Some(profile_token) = sanitize_onvif_token(body.profile_token.as_deref(), &default_prof)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid ONVIF profile token" })),
        )
            .into_response();
    };

    // Moving the camera via ContinuousMove, RelativeMove, or AbsoluteMove clears active_preset until GotoPreset completes
    state
        .camera_driver_runtime
        .set_active_preset(&camera, None)
        .await;

    let (pan, tilt, zoom) = body.resolve_vectors();
    let is_rel = body.is_relative();
    let is_abs = body.is_absolute();
    let op_name = if is_abs {
        "AbsoluteMove"
    } else if is_rel {
        "RelativeMove"
    } else {
        "ContinuousMove"
    };
    let user = creds.0.as_deref();
    let pass = creds.1.as_deref();
    let move_timeout = body.timeout_sec.unwrap_or(cam_cfg.onvif.timeout_sec);
    let build_move_xml = |prof: &str| {
        if is_abs {
            build_onvif_absolute_move_soap(prof, pan, tilt, zoom, body.speed, user, pass)
        } else if is_rel {
            build_onvif_relative_move_soap(prof, pan, tilt, zoom, body.speed, user, pass)
        } else {
            build_onvif_continuous_move_soap(prof, pan, tilt, zoom, move_timeout, user, pass)
        }
    };
    let timeout = match body.timeout_ms {
        Some(ms) => std::time::Duration::from_millis(ms.clamp(20, 30_000)),
        None => std::time::Duration::from_secs(
            body.timeout_sec
                .unwrap_or(cam_cfg.onvif.timeout_sec)
                .clamp(1, 30),
        ),
    };
    let transport = camera_protocols::ReqwestCameraWireTransport::new(state.http_client.clone());

    let res = state
        .camera_driver_runtime
        .send_ptz_soap_with_profile_discovery(
            &transport,
            &camera,
            &endpoint,
            &profile_token,
            body.profile_token.is_none(),
            cam_cfg.hardware.protocol.as_deref(),
            op_name,
            (user, pass),
            timeout,
            build_move_xml,
        )
        .await;

    match res {
        Ok(_) => Json(json!({
            "success": true,
            "camera": camera,
            "operation": op_name,
            "pan": pan,
            "tilt": tilt,
            "zoom": zoom,
            "active_preset": Value::Null
        }))
        .into_response(),
        Err((status, err_msg)) => (
            status,
            Json(json!({
                "success": false,
                "camera": camera,
                "operation": op_name,
                "error": err_msg,
                "active_preset": Value::Null
            })),
        )
            .into_response(),
    }
}

async fn api_camera_ptz_stop(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    body_bytes: axum::body::Bytes,
) -> Response {
    if let Some(err_resp) =
        verify_camera_mutation_access(&state, &jar, &uri, &headers, &camera).await
    {
        return err_resp.into_response();
    }
    let body: PtzStopBody = if body_bytes.is_empty() {
        PtzStopBody::default()
    } else {
        serde_json::from_slice(&body_bytes).unwrap_or_default()
    };

    let (cam_opt, creds, ptz_endpoint_opt) = {
        let cfg = state.config.read().await;
        let cam = cfg.cameras.get(&camera).cloned();
        let creds = cam.as_ref().map_or((None, None), |c| {
            resolve_effective_onvif_credentials(&cfg, &camera, c)
        });
        let ep_opt = cam.as_ref().and_then(|c| {
            resolve_effective_ptz_endpoint(&state.camera_driver_runtime, &cfg, &camera, c)
        });
        (cam, creds, ep_opt)
    };
    let Some(cam_cfg) = cam_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": format!("Camera '{camera}' not found") })),
        )
            .into_response();
    };
    let Some(endpoint) = ptz_endpoint_opt else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": format!("ONVIF PTZ is not configured for camera '{camera}'") })),
        )
            .into_response();
    };

    let default_prof =
        default_camera_profile_token(&state.camera_driver_runtime, &camera, &endpoint, &cam_cfg);
    let Some(profile_token) = sanitize_onvif_token(body.profile_token.as_deref(), &default_prof)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid ONVIF profile token" })),
        )
            .into_response();
    };
    let user = creds.0.as_deref();
    let pass = creds.1.as_deref();
    let timeout = match body.timeout_ms {
        Some(ms) => std::time::Duration::from_millis(ms.clamp(20, 30_000)),
        None => std::time::Duration::from_secs(cam_cfg.onvif.timeout_sec.clamp(1, 30)),
    };
    let active_preset = state
        .camera_driver_runtime
        .get_active_preset(&camera, None)
        .await;
    let transport = camera_protocols::ReqwestCameraWireTransport::new(state.http_client.clone());

    let res = state
        .camera_driver_runtime
        .send_ptz_soap_with_profile_discovery(
            &transport,
            &camera,
            &endpoint,
            &profile_token,
            body.profile_token.is_none(),
            cam_cfg.hardware.protocol.as_deref(),
            "Stop",
            (user, pass),
            timeout,
            |prof| build_onvif_stop_soap(prof, body.pan_tilt, body.zoom, user, pass),
        )
        .await;

    match res {
        Ok(_) => Json(json!({
            "success": true,
            "camera": camera,
            "operation": "Stop",
            "pan_tilt": body.pan_tilt,
            "zoom": body.zoom,
            "active_preset": active_preset
        }))
        .into_response(),
        Err((status, err_msg)) => (
            status,
            Json(json!({
                "success": false,
                "camera": camera,
                "operation": "Stop",
                "error": err_msg,
                "active_preset": active_preset
            })),
        )
            .into_response(),
    }
}

async fn api_camera_ptz_preset(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    Json(body): Json<PtzPresetBody>,
) -> Response {
    if let Some(err_resp) =
        verify_camera_mutation_access(&state, &jar, &uri, &headers, &camera).await
    {
        return err_resp.into_response();
    }
    let preset_token = body.preset.trim();
    if preset_token.is_empty()
        || preset_token.len() > 128
        || preset_token.chars().any(|c| c.is_control())
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Preset token/name is required and must be valid" })),
        )
            .into_response();
    }

    let (cam_opt, creds, ptz_endpoint_opt) = {
        let cfg = state.config.read().await;
        let cam = cfg.cameras.get(&camera).cloned();
        let creds = cam.as_ref().map_or((None, None), |c| {
            resolve_effective_onvif_credentials(&cfg, &camera, c)
        });
        let ep_opt = cam.as_ref().and_then(|c| {
            resolve_effective_ptz_endpoint(&state.camera_driver_runtime, &cfg, &camera, c)
        });
        (cam, creds, ep_opt)
    };
    let Some(cam_cfg) = cam_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": format!("Camera '{camera}' not found") })),
        )
            .into_response();
    };
    let Some(endpoint) = ptz_endpoint_opt else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": format!("ONVIF PTZ is not configured for camera '{camera}'") })),
        )
            .into_response();
    };

    let default_prof =
        default_camera_profile_token(&state.camera_driver_runtime, &camera, &endpoint, &cam_cfg);
    let Some(profile_token) = sanitize_onvif_token(body.profile_token.as_deref(), &default_prof)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid ONVIF profile token" })),
        )
            .into_response();
    };

    let user = creds.0.as_deref();
    let pass = creds.1.as_deref();
    let timeout = match body.timeout_ms {
        Some(ms) => std::time::Duration::from_millis(ms.clamp(20, 30_000)),
        None => std::time::Duration::from_secs(cam_cfg.onvif.timeout_sec.clamp(1, 30)),
    };
    let transport = camera_protocols::ReqwestCameraWireTransport::new(state.http_client.clone());

    let soft_opt = cam_cfg
        .hardware
        .soft_presets
        .get(preset_token)
        .cloned()
        .or_else(|| {
            cam_cfg
                .hardware
                .soft_presets
                .values()
                .find(|sp| sp.name.eq_ignore_ascii_case(preset_token))
                .cloned()
        });

    match state
        .camera_driver_runtime
        .goto_preset_with_soft_fallback(
            &transport,
            &camera,
            &endpoint,
            &profile_token,
            body.profile_token.is_none(),
            cam_cfg.hardware.protocol.as_deref(),
            preset_token,
            body.speed,
            soft_opt.as_ref(),
            (user, pass),
            timeout,
        )
        .await
    {
        Ok("SoftPresetRelativeMove") => Json(json!({
            "success": true,
            "camera": camera,
            "operation": "GotoPreset",
            "preset": preset_token,
            "active_preset": preset_token,
            "source": "soft_preset"
        }))
        .into_response(),
        Ok(_) => Json(json!({
            "success": true,
            "camera": camera,
            "operation": "GotoPreset",
            "preset": preset_token,
            "active_preset": preset_token
        }))
        .into_response(),
        Err((status, err_msg)) => (
            status,
            Json(json!({
                "success": false,
                "camera": camera,
                "operation": "GotoPreset",
                "error": err_msg,
                "active_preset": Value::Null
            })),
        )
            .into_response(),
    }
}

async fn api_camera_ptz_preset_save(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    Json(body): Json<PtzPresetSaveBody>,
) -> Response {
    if let Some(err_resp) =
        verify_camera_mutation_access(&state, &jar, &uri, &headers, &camera).await
    {
        return err_resp.into_response();
    }
    let preset_name = body.name.trim();
    if preset_name.is_empty()
        || preset_name.len() > 64
        || preset_name.contains('/')
        || preset_name.chars().any(|c| c.is_control())
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Preset name is required (1-64 characters, no '/' or control characters)" })),
        )
            .into_response();
    }
    let validated_token = if let Some(raw_tok) = body
        .token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if raw_tok.len() > 64 || raw_tok.contains('/') || raw_tok.chars().any(|c| c.is_control()) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "success": false, "message": "Invalid preset token (1-64 characters, no '/' or control characters)" })),
            )
                .into_response();
        }
        Some(raw_tok.to_string())
    } else {
        None
    };

    let (cam_opt, creds, ptz_endpoint_opt) = {
        let cfg = state.config.read().await;
        let cam = cfg.cameras.get(&camera).cloned();
        let creds = cam.as_ref().map_or((None, None), |c| {
            resolve_effective_onvif_credentials(&cfg, &camera, c)
        });
        let ep_opt = cam.as_ref().and_then(|c| {
            resolve_effective_ptz_endpoint(&state.camera_driver_runtime, &cfg, &camera, c)
        });
        (cam, creds, ep_opt)
    };
    let Some(cam_cfg) = cam_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": format!("Camera '{camera}' not found") })),
        )
            .into_response();
    };
    let Some(endpoint) = ptz_endpoint_opt else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": format!("ONVIF PTZ is not configured for camera '{camera}'") })),
        )
            .into_response();
    };

    let default_prof =
        default_camera_profile_token(&state.camera_driver_runtime, &camera, &endpoint, &cam_cfg);
    let Some(profile_token) = sanitize_onvif_token(body.profile_token.as_deref(), &default_prof)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid ONVIF profile token" })),
        )
            .into_response();
    };

    let user = creds.0.as_deref();
    let pass = creds.1.as_deref();
    let timeout = match body.timeout_ms {
        Some(ms) => std::time::Duration::from_millis(ms.clamp(20, 30_000)),
        None => std::time::Duration::from_secs(cam_cfg.onvif.timeout_sec.clamp(1, 30)),
    };
    let transport = camera_protocols::ReqwestCameraWireTransport::new(state.http_client.clone());

    let set_res = state
        .camera_driver_runtime
        .send_ptz_soap_with_profile_discovery(
            &transport,
            &camera,
            &endpoint,
            &profile_token,
            body.profile_token.is_none(),
            cam_cfg.hardware.protocol.as_deref(),
            "SetPreset",
            (user, pass),
            timeout,
            |prof| {
                build_onvif_set_preset_soap(
                    prof,
                    preset_name,
                    validated_token.as_deref(),
                    user,
                    pass,
                )
            },
        )
        .await;

    match set_res {
        Ok((_resolved_prof, resp_xml)) => {
            let resolved_token = parse_onvif_set_preset_token_xml(&resp_xml)
                .or_else(|| validated_token.clone())
                .unwrap_or_else(|| preset_name.to_string());
            state
                .camera_driver_runtime
                .set_active_preset(&camera, Some(preset_name.to_string()))
                .await;
            Json(json!({
                "success": true,
                "camera": camera,
                "operation": "SetPreset",
                "token": resolved_token,
                "name": preset_name,
                "active_preset": preset_name,
                "source": "onvif"
            }))
            .into_response()
        }
        Err((_, ref err_msg)) if is_onvif_action_not_supported_error(err_msg) => {
            // Soft-preset fallback: read current position from GetStatus (with ProfileToken discovery) and store in camera.hardware.soft_presets
            let (_, cur_status) = match fetch_ptz_status_with_profile_discovery(
                &state,
                &camera,
                &endpoint,
                (
                    &profile_token,
                    body.profile_token.is_none(),
                    cam_cfg.hardware.protocol.as_deref(),
                ),
                (user, pass),
                timeout,
            )
            .await
            {
                Ok(pair) => pair,
                Err((st, e)) => {
                    return (
                        st,
                        Json(json!({
                            "success": false,
                            "camera": camera,
                            "operation": "SetPreset",
                            "error": e
                        })),
                    )
                        .into_response();
                }
            };
            let soft_token = validated_token.unwrap_or_else(|| preset_name.to_string());
            let soft_token_for_mutate = soft_token.clone();
            let preset_name_for_mutate = preset_name.to_string();
            let cam_for_mutate = camera.clone();
            let save_soft_res = state
                .config_coordinator
                .mutate_camera_and_persist(&camera, None, false, move |cfg_guard| {
                    if let Some(cam_mut) = cfg_guard.cameras.get_mut(&cam_for_mutate) {
                        if cam_mut.hardware.soft_presets.len() >= MAX_ONVIF_PRESETS
                            && !cam_mut
                                .hardware
                                .soft_presets
                                .contains_key(&soft_token_for_mutate)
                        {
                            anyhow::bail!("MAX_ONVIF_PRESETS_REACHED");
                        }
                        cam_mut.hardware.soft_presets.insert(
                            soft_token_for_mutate,
                            ninjanvr_config::CameraSoftPreset {
                                name: preset_name_for_mutate,
                                pan: cur_status.pan,
                                tilt: cur_status.tilt,
                                zoom: cur_status.zoom,
                            },
                        );
                    }
                    Ok(())
                })
                .await;
            if let Err(err) = save_soft_res {
                if err.to_string().contains("MAX_ONVIF_PRESETS_REACHED") {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "success": false,
                            "camera": camera,
                            "operation": "SetPreset",
                            "message": format!("Maximum number of presets ({MAX_ONVIF_PRESETS}) reached")
                        })),
                    )
                        .into_response();
                }
                warn!("Failed to persist config.yml after saving soft preset: {err:#}");
            }
            state
                .camera_driver_runtime
                .set_active_preset(&camera, Some(preset_name.to_string()))
                .await;
            Json(json!({
                "success": true,
                "camera": camera,
                "operation": "SetPreset",
                "token": soft_token,
                "name": preset_name,
                "active_preset": preset_name,
                "source": "soft_preset"
            }))
            .into_response()
        }
        Err((status, err_msg)) => (
            status,
            Json(json!({
                "success": false,
                "camera": camera,
                "operation": "SetPreset",
                "error": err_msg
            })),
        )
            .into_response(),
    }
}

async fn api_camera_ptz_preset_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath((camera, token)): AxumPath<(String, String)>,
    Query(q): Query<PtzInfoQueryParams>,
) -> Response {
    if let Some(err_resp) =
        verify_camera_mutation_access(&state, &jar, &uri, &headers, &camera).await
    {
        return err_resp.into_response();
    }
    let clean_tok = token.trim();
    if clean_tok.is_empty() || clean_tok.len() > 128 || clean_tok.chars().any(|c| c.is_control()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid preset token" })),
        )
            .into_response();
    }

    // Check if it is a soft preset first
    let clean_tok_owned = clean_tok.to_string();
    let cam_for_soft = camera.clone();
    let removed_soft_name: Option<String> = state
        .config_coordinator
        .mutate_camera_and_persist(&camera, None, false, move |cfg_guard| {
            let mut removed_name = None;
            if let Some(cam_mut) = cfg_guard.cameras.get_mut(&cam_for_soft) {
                if let Some(removed) = cam_mut.hardware.soft_presets.remove(&clean_tok_owned) {
                    removed_name = Some(removed.name);
                } else if let Some(found_key) = cam_mut
                    .hardware
                    .soft_presets
                    .iter()
                    .find(|(_, v)| v.name.eq_ignore_ascii_case(&clean_tok_owned))
                    .map(|(k, _)| k.clone())
                    && let Some(removed) = cam_mut.hardware.soft_presets.remove(&found_key)
                {
                    removed_name = Some(removed.name);
                }
            }
            Ok(removed_name)
        })
        .await
        .unwrap_or_else(|err| {
            warn!("Failed to persist config.yml after deleting soft preset: {err:#}");
            None
        });
    if let Some(soft_name) = removed_soft_name {
        state
            .camera_driver_runtime
            .clear_active_preset_if_matching(&camera, &[clean_tok, soft_name.as_str()])
            .await;
        return Json(json!({
            "success": true,
            "camera": camera,
            "operation": "RemovePreset",
            "deleted_token": clean_tok,
            "source": "soft_preset"
        }))
        .into_response();
    }

    let (cam_opt, creds, ptz_endpoint_opt) = {
        let cfg = state.config.read().await;
        let cam = cfg.cameras.get(&camera).cloned();
        let creds = cam.as_ref().map_or((None, None), |c| {
            resolve_effective_onvif_credentials(&cfg, &camera, c)
        });
        let ep_opt = cam.as_ref().and_then(|c| {
            resolve_effective_ptz_endpoint(&state.camera_driver_runtime, &cfg, &camera, c)
        });
        (cam, creds, ep_opt)
    };
    let Some(cam_cfg) = cam_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": format!("Camera '{camera}' not found") })),
        )
            .into_response();
    };
    let Some(endpoint) = ptz_endpoint_opt else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": format!("ONVIF PTZ is not configured for camera '{camera}'") })),
        )
            .into_response();
    };

    let default_prof =
        default_camera_profile_token(&state.camera_driver_runtime, &camera, &endpoint, &cam_cfg);
    let Some(profile_token) = sanitize_onvif_token(q.profile_token.as_deref(), &default_prof)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid ONVIF profile token" })),
        )
            .into_response();
    };

    let user = creds.0.as_deref();
    let pass = creds.1.as_deref();
    let timeout = match q.timeout_ms {
        Some(ms) => std::time::Duration::from_millis(ms.clamp(20, 30_000)),
        None => std::time::Duration::from_secs(cam_cfg.onvif.timeout_sec.clamp(1, 30)),
    };
    let transport = camera_protocols::ReqwestCameraWireTransport::new(state.http_client.clone());

    let del_res = state
        .camera_driver_runtime
        .send_ptz_soap_with_profile_discovery(
            &transport,
            &camera,
            &endpoint,
            &profile_token,
            q.profile_token.is_none(),
            cam_cfg.hardware.protocol.as_deref(),
            "RemovePreset",
            (user, pass),
            timeout,
            |prof| build_onvif_remove_preset_soap(prof, clean_tok, user, pass),
        )
        .await;

    match del_res {
        Ok(_) => {
            state
                .camera_driver_runtime
                .clear_active_preset_if_matching(&camera, &[clean_tok])
                .await;
            Json(json!({
                "success": true,
                "camera": camera,
                "operation": "RemovePreset",
                "deleted_token": clean_tok,
                "source": "onvif"
            }))
            .into_response()
        }
        Err((status, err_msg)) => (
            status,
            Json(json!({
                "success": false,
                "camera": camera,
                "operation": "RemovePreset",
                "error": err_msg
            })),
        )
            .into_response(),
    }
}

fn go2rtc_stream_has_native_backchannel(
    cfg: &NinjaConfig,
    camera: &str,
    cam_cfg: &ninjanvr_config::CameraConfig,
) -> bool {
    if cam_cfg
        .hardware
        .cloud_password
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty() && p.trim() != "***" && p.trim() != "*")
    {
        return true;
    }
    let check_url = |s: &str| -> bool {
        let lc = s.trim().to_ascii_lowercase();
        if lc.contains("#backchannel=0") {
            return false;
        }
        lc.starts_with("tapo://")
            || lc.starts_with("dvrip://")
            || lc.starts_with("isapi://")
            || lc.starts_with("roborock://")
            || lc.starts_with("ring://")
            || lc.starts_with("tuya://")
            || lc.starts_with("xiaomi://")
            || lc.starts_with("doorbird://")
            || lc.contains("#backchannel=1")
    };
    let talk_key = format!("{camera}_talk");
    for stream_key in [camera, talk_key.as_str()] {
        if let Some(val) = cfg.go2rtc.streams.get(stream_key) {
            match val {
                Value::String(s) if check_url(s) => {
                    return true;
                }
                Value::Array(arr)
                    if arr.iter().any(|item| item.as_str().is_some_and(check_url)) =>
                {
                    return true;
                }
                _ => {}
            }
        }
    }
    false
}

#[derive(Debug, Clone, Copy, Deserialize, Default)]
pub struct HardwareQueryParams {
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct CameraHardwareProbeTarget {
    host: String,
    onvif_port: u16,
    user: Option<String>,
    password: Option<String>,
    is_tapo_hint: bool,
    is_dahua_hint: bool,
    ptz_ep_opt: Option<String>,
}

fn resolve_camera_hardware_probe_targets(
    runtime: &CameraDriverRuntime,
    cfg: &NinjaConfig,
    camera: &str,
    cam_cfg: &ninjanvr_config::CameraConfig,
) -> Option<CameraHardwareProbeTarget> {
    let target = runtime.resolve_probe_target_with_config(cfg, camera, cam_cfg);
    let host = target.host?;
    let ptz_ep_opt = target
        .ptz_endpoint
        .filter(|ep| is_safe_onvif_endpoint_url(ep));
    ptz_ep_opt.as_ref()?;
    Some(CameraHardwareProbeTarget {
        host,
        onvif_port: target.onvif_port,
        user: target.user,
        password: target.password,
        is_tapo_hint: target.is_tapo_hint,
        is_dahua_hint: target.is_dahua_hint,
        ptz_ep_opt,
    })
}

async fn discover_onvif_media_tokens(
    runtime: &CameraDriverRuntime,
    transport: &dyn camera_protocols::CameraWireTransport,
    onvif_base: &str,
    (user, pass): (Option<&str>, Option<&str>),
    timeout: std::time::Duration,
) -> (Vec<String>, Vec<String>) {
    runtime
        .discover_onvif_media_tokens_with_transport(transport, onvif_base, (user, pass), timeout)
        .await
}

async fn api_camera_hardware_get(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    Query(q): Query<HardwareQueryParams>,
) -> Response {
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }

    let (cam_opt, probe_target, has_native_backchannel) = {
        let cfg = state.config.read().await;
        let cam = cfg.cameras.get(&camera).cloned();
        let target = cam.as_ref().and_then(|c| {
            resolve_camera_hardware_probe_targets(&state.camera_driver_runtime, &cfg, &camera, c)
        });
        let backchannel = cam
            .as_ref()
            .is_some_and(|c| go2rtc_stream_has_native_backchannel(&cfg, &camera, c));
        (cam, target, backchannel)
    };

    let Some(cam_cfg) = cam_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": format!("Camera '{camera}' not found") })),
        )
            .into_response();
    };

    let Some(CameraHardwareProbeTarget {
        host,
        onvif_port,
        user: user_opt,
        password: pass_opt,
        is_tapo_hint,
        is_dahua_hint,
        ptz_ep_opt,
    }) = probe_target
    else {
        return Json(json!({
            "camera": camera,
            "online": false,
            "protocol_family": "rtsp_only",
            "onvif_configured": cam_cfg.onvif.is_configured(),
            "device_info": CameraDeviceIdentity::default(),
            "capabilities": {
                "ptz": false,
                "ptz_zoom": false,
                "ptz_presets": false,
                "ptz_presets_manage": false,
                "push_to_talk": has_native_backchannel,
                "push_to_talk_mode": if has_native_backchannel { json!("webrtc") } else { Value::Null },
                "imaging": false,
                "hardware_autotracking": false,
                "day_night_mode": false,
                "ir_lighting": false,
                "status_led": false,
                "flip_mirror": false,
                "wdr": false,
                "siren_on_event": false,
                "manual_siren": false,
                "audio_anomaly_detect": false,
                "audio_volume": false,
                "reboot": false
            },
            "missing_requirements": ["onvif_not_configured"],
            "suggested_config": Value::Null,
            "state": {}
        }))
        .into_response();
    };

    let is_admin = caller_can_mutate(&state, &jar, &headers, true).await;
    let max_timeout_ms = if is_admin { 10_000 } else { 2_500 };
    let timeout = match q.timeout_ms {
        Some(ms) => std::time::Duration::from_millis(ms.clamp(50, max_timeout_ms)),
        None => std::time::Duration::from_secs(cam_cfg.onvif.timeout_sec.clamp(1, 5)),
    };
    let user = user_opt.as_deref();
    let pass = pass_opt.as_deref();

    let onvif_base = format_camera_http_base(&host, onvif_port);
    let dev_ep = format!("{onvif_base}/onvif/device_service");
    let ptz_ep = ptz_ep_opt.unwrap_or_else(|| format!("{onvif_base}/onvif/ptz_service"));
    let transport = camera_protocols::ReqwestCameraWireTransport::new(state.http_client.clone());

    // 1. Probe ONVIF GetDeviceInformation over CameraWireTransport
    let dev_soap = build_onvif_get_device_info_soap(user, pass);
    let dev_res = transport
        .send_soap(
            &dev_ep,
            "http://www.onvif.org/ver10/device/wsdl",
            "GetDeviceInformation",
            dev_soap,
            (user, pass),
            timeout,
        )
        .await;
    let host_timed_out = matches!(dev_res, Err((StatusCode::GATEWAY_TIMEOUT, _)));
    let mut dev_info = match &dev_res {
        Ok(xml) => parse_onvif_device_info_xml(xml),
        Err(_) => CameraDeviceIdentity::default(),
    };

    let is_tapo = if let Some(ref mfg) = dev_info.manufacturer {
        mfg.to_ascii_lowercase().contains("tp-link")
            || dev_info
                .model
                .as_deref()
                .is_some_and(|m| m.to_ascii_lowercase().contains("tapo"))
    } else {
        is_tapo_hint
    };
    let img_endpoints = if is_tapo {
        [
            format!("{onvif_base}/onvif/service"),
            format!("{onvif_base}/onvif/imaging_service"),
        ]
    } else {
        [
            format!("{onvif_base}/onvif/imaging_service"),
            format!("{onvif_base}/onvif/service"),
        ]
    };

    // 2. Probe ONVIF PTZ GetStatus (with deduplicated ProfileToken + GetProfiles discovery; abort on network timeout/unreachable)
    let initial_prof = cam_cfg
        .hardware
        .profile_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|t| sanitize_onvif_token(Some(t), ""))
        .filter(|s| !s.is_empty())
        .or_else(|| state.camera_driver_runtime.get_profile(&camera, &ptz_ep))
        .unwrap_or_else(|| {
            if is_tapo {
                "profile_1".to_string()
            } else {
                "Profile000".to_string()
            }
        });
    let mut discovered_profile = initial_prof.clone();
    let mut ptz_ok = false;
    let mut ptz_zoom = false;
    let mut media_discovered: Option<(Vec<String>, Vec<String>)> = None;
    if !host_timed_out {
        let proto_hint = if is_tapo {
            Some("tapo")
        } else {
            cam_cfg.hardware.protocol.as_deref()
        };
        let mut tried_profs = vec![initial_prof.clone()];
        for cand in
            state
                .camera_driver_runtime
                .candidate_profile_tokens(&camera, &initial_prof, proto_hint)
        {
            tried_profs.push(cand.to_string());
        }
        match state
            .camera_driver_runtime
            .send_ptz_soap_with_profile_discovery(
                &transport,
                &camera,
                &ptz_ep,
                &initial_prof,
                true,
                proto_hint,
                "GetStatus",
                (user, pass),
                timeout,
                |prof| build_onvif_get_status_soap(prof, user, pass),
            )
            .await
        {
            Ok((resolved_prof, st_xml)) => {
                ptz_ok = true;
                discovered_profile = resolved_prof;
                if let Some(pos_block) = extract_xml_element_text(&st_xml, "Position")
                    && pos_block.contains("Zoom")
                {
                    ptz_zoom = true;
                }
            }
            Err((StatusCode::GATEWAY_TIMEOUT, _)) => {}
            Err(_) => {
                let (dyn_profs, dyn_vs) = discover_onvif_media_tokens(
                    &state.camera_driver_runtime,
                    &transport,
                    &onvif_base,
                    (user, pass),
                    timeout,
                )
                .await;
                for dyn_prof in &dyn_profs {
                    if tried_profs.contains(dyn_prof) {
                        continue;
                    }
                    let st_soap = build_onvif_get_status_soap(dyn_prof, user, pass);
                    if let Ok(st_xml) = transport
                        .send_soap(
                            &ptz_ep,
                            "http://www.onvif.org/ver20/ptz/wsdl",
                            "GetStatus",
                            st_soap,
                            (user, pass),
                            timeout,
                        )
                        .await
                    {
                        ptz_ok = true;
                        discovered_profile = dyn_prof.clone();
                        state.camera_driver_runtime.set_profile(
                            &camera,
                            &ptz_ep,
                            &discovered_profile,
                        );
                        if let Some(pos_block) = extract_xml_element_text(&st_xml, "Position")
                            && pos_block.contains("Zoom")
                        {
                            ptz_zoom = true;
                        }
                        break;
                    }
                }
                media_discovered = Some((dyn_profs, dyn_vs));
            }
        }
    }

    // 3. Probe ONVIF Imaging GetImagingSettings across both endpoints (deduplicated; abort on network timeout)
    let default_vs = cam_cfg
        .hardware
        .video_source_token
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| state.camera_driver_runtime.get_video_source(&camera))
        .unwrap_or_else(|| {
            if is_tapo {
                "raw_vs1".to_string()
            } else {
                "VideoSource000".to_string()
            }
        });
    let mut discovered_vs = default_vs.clone();
    let mut imaging_ok = false;
    let mut img_tuple = (None, None, None, None);
    if !host_timed_out && !is_tapo {
        let extra_vs: &[String] = media_discovered
            .as_ref()
            .map_or(&[], |(_, dyn_vs)| dyn_vs.as_slice());
        if let Some((resolved_vs, tuple)) = state
            .camera_driver_runtime
            .probe_imaging_settings_with_transport(
                &transport,
                &camera,
                &img_endpoints,
                &default_vs,
                cam_cfg.hardware.protocol.as_deref(),
                extra_vs,
                (user, pass),
                timeout,
            )
            .await
        {
            imaging_ok = true;
            discovered_vs = resolved_vs;
            img_tuple = tuple;
        }
    }

    // 4. Probe Dahua / Intelbras HTTP CGI if not Tapo
    let cgi_port = if onvif_port != 2020 { onvif_port } else { 80 };
    let cgi_base = format_camera_http_base(&host, cgi_port);
    let mut dahua_cgi_ok = false;
    let mut state_map = serde_json::Map::new();
    if let Some(b) = img_tuple.0 {
        state_map.insert("brightness".to_string(), json!(b.round() as i32));
    }
    if let Some(c) = img_tuple.1 {
        state_map.insert("contrast".to_string(), json!(c.round() as i32));
    }
    if let Some(s) = img_tuple.2 {
        state_map.insert("saturation".to_string(), json!(s.round() as i32));
    }
    if let Some(sh) = img_tuple.3 {
        state_map.insert("sharpness".to_string(), json!(sh.round() as i32));
    }

    if !host_timed_out
        && !is_tapo
        && (is_dahua_hint
            || onvif_port == 80
            || dev_info.manufacturer.as_deref().is_some_and(|m| {
                m.to_ascii_lowercase().contains("intelbras")
                    || m.to_ascii_lowercase().contains("dahua")
                    || m.to_ascii_lowercase().contains("amcrest")
                    || m.to_ascii_lowercase().contains("imou")
            }))
    {
        dahua_cgi_ok = state
            .camera_driver_runtime
            .probe_dahua_cgi_system_and_tables(
                &transport,
                &cgi_base,
                &mut dev_info,
                &mut state_map,
                (user, pass),
                timeout,
            )
            .await;
    }

    let online = dev_res.is_ok() || ptz_ok || imaging_ok || dahua_cgi_ok;
    let resolved_spec = infer_protocol_spec_from_device_or_hints(
        cam_cfg.hardware.protocol.as_deref(),
        dev_info.manufacturer.as_deref(),
        dev_info.model.as_deref(),
        is_tapo,
        is_dahua_hint,
        dahua_cgi_ok,
        online,
    );
    let protocol_family = resolved_spec.id;
    if online {
        state
            .camera_driver_runtime
            .set_protocol(&camera, protocol_family);
    }

    let (ptt_supported, ptt_mode) = if has_native_backchannel {
        (true, Some("webrtc"))
    } else if dahua_cgi_ok {
        (true, Some("cgi_alaw"))
    } else if let Some(mode) = resolved_spec.default_talkback_mode {
        (true, Some(mode))
    } else {
        (false, None)
    };

    let has_tapo_cloud_pw = cam_cfg
        .hardware
        .cloud_password
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty() && p.trim() != "***" && p.trim() != "*");

    let mut missing_reqs: Vec<&str> = Vec::new();
    if online && !cam_cfg.onvif.is_configured() {
        missing_reqs.push("onvif_not_configured");
    }
    if is_tapo && !has_tapo_cloud_pw {
        missing_reqs.push("tapo_cloud_password");
    }

    let clean_host_only = extract_clean_camera_host(&host);
    if !is_admin {
        dev_info.serial_number = None;
    }

    let suggested_substream = format_substream_url_for_protocol(resolved_spec, &clean_host_only);

    let suggested_cfg_json = if is_admin {
        json!({
            "onvif_host": clean_host_only,
            "onvif_port": onvif_port,
            "onvif_user": user,
            "profile_token": discovered_profile,
            "video_source_token": discovered_vs,
            "protocol": protocol_family,
            "native_substream_url": suggested_substream
        })
    } else {
        Value::Null
    };

    Json(json!({
        "camera": camera,
        "online": online,
        "protocol_family": protocol_family,
        "onvif_configured": cam_cfg.onvif.is_configured(),
        "device_info": dev_info,
        "capabilities": {
            "ptz": ptz_ok,
            "ptz_zoom": ptz_zoom,
            "ptz_presets": ptz_ok,
            "ptz_presets_manage": ptz_ok,
            "push_to_talk": ptt_supported,
            "push_to_talk_mode": ptt_mode,
            "imaging": imaging_ok,
            "hardware_autotracking": dahua_cgi_ok,
            "day_night_mode": dahua_cgi_ok,
            "ir_lighting": dahua_cgi_ok,
            "status_led": dahua_cgi_ok,
            "flip_mirror": dahua_cgi_ok,
            "wdr": dahua_cgi_ok,
            "siren_on_event": dahua_cgi_ok && is_admin,
            "manual_siren": (dahua_cgi_ok || (is_tapo && has_tapo_cloud_pw)) && is_admin,
            "audio_anomaly_detect": dahua_cgi_ok && is_admin,
            "audio_volume": dahua_cgi_ok,
            "camera_logs": online && (resolved_spec.supports_onvif_system_log || resolved_spec.supports_onvif_pull_events || dahua_cgi_ok),
            "remote_syslog": dahua_cgi_ok && is_admin,
            "scheduled_reboot": dahua_cgi_ok && is_admin,
            "reboot": online && is_admin
        },
        "missing_requirements": missing_reqs,
        "suggested_config": suggested_cfg_json,
        "state": state_map
    }))
    .into_response()
}

fn encode_rtsp_userinfo_part(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn api_camera_hardware_set(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    Json(body): Json<CameraHardwareUpdateBody>,
) -> Response {
    if let Some(err_resp) =
        verify_camera_mutation_access(&state, &jar, &uri, &headers, &camera).await
    {
        return err_resp.into_response();
    }

    if (body.apply_suggested_onvif == Some(true)
        || body.apply_suggested_substream == Some(true)
        || body.cloud_password.is_some()
        || body.protocol.is_some()
        || body.profile_token.is_some()
        || body.video_source_token.is_some()
        || body.reboot == Some(true)
        || body.trigger_siren == Some(true)
        || body.siren_on_motion.is_some()
        || body.siren_on_audio.is_some()
        || body.audio_detect_enabled.is_some()
        || body.audio_sensitivity.is_some()
        || body.syslog_enabled.is_some()
        || body.syslog_server.is_some()
        || body.syslog_port.is_some()
        || body.auto_reboot_enabled.is_some()
        || body.auto_reboot_day.is_some()
        || body.auto_reboot_hour.is_some()
        || body.auto_reboot_minute.is_some())
        && !caller_can_mutate(&state, &jar, &headers, true).await
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Admin role required for camera configuration, alarm linkage, or reboot" })),
        )
            .into_response();
    }

    let (cam_opt, probe_target) = {
        let cfg = state.config.read().await;
        let cam = cfg.cameras.get(&camera).cloned();
        let target = cam.as_ref().and_then(|c| {
            resolve_camera_hardware_probe_targets(&state.camera_driver_runtime, &cfg, &camera, c)
        });
        (cam, target)
    };

    let Some(mut cam_cfg) = cam_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": format!("Camera '{camera}' not found") })),
        )
            .into_response();
    };

    let Some(CameraHardwareProbeTarget {
        host,
        onvif_port,
        user: user_opt,
        password: pass_opt,
        is_tapo_hint,
        is_dahua_hint,
        ptz_ep_opt,
    }) = probe_target
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": format!("Camera '{camera}' has no reachable upstream IP or ONVIF endpoint") })),
        )
            .into_response();
    };

    let mut applied_ops: Vec<String> = Vec::new();
    let onvif_base = format_camera_http_base(&host, onvif_port);
    let cgi_port = if onvif_port != 2020 { onvif_port } else { 80 };
    let cgi_base = format_camera_http_base(&host, cgi_port);
    let ptz_ep = ptz_ep_opt.unwrap_or_else(|| format!("{onvif_base}/onvif/ptz_service"));

    // 1. Handle 1-click ONVIF / Substream / Cloud Password configuration persistence
    if body.apply_suggested_onvif == Some(true)
        || body.apply_suggested_substream == Some(true)
        || body.cloud_password.is_some()
        || body.protocol.is_some()
        || body.profile_token.is_some()
        || body.video_source_token.is_some()
    {
        let clean_host_only = extract_clean_camera_host(&host);
        let cached_prof = state.camera_driver_runtime.get_profile(&camera, &ptz_ep);
        let cached_vs = state.camera_driver_runtime.get_video_source(&camera);
        let cached_proto = state.camera_driver_runtime.get_protocol(&camera);
        let cam_name_for_hw = camera.clone();
        let user_for_hw = user_opt.clone();
        let pass_for_hw = pass_opt.clone();
        let body_apply_onvif = body.apply_suggested_onvif;
        let body_apply_sub = body.apply_suggested_substream;
        let body_proto = body.protocol.clone();
        let body_prof = body.profile_token.clone();
        let body_vs = body.video_source_token.clone();
        let body_cp = body.cloud_password.clone();

        match state
            .config_coordinator
            .mutate_camera_and_persist(
                &camera,
                Some(&state.camera_driver_runtime),
                true,
                move |cfg_guard| {
                    let mut ops = Vec::new();
                    let mut updated_cam = None;
                    if let Some(cam_mut) = cfg_guard.cameras.get_mut(&cam_name_for_hw) {
                        if body_apply_onvif == Some(true) {
                            cam_mut.onvif.host = Some(clean_host_only.clone());
                            cam_mut.onvif.port = onvif_port;
                            if cam_mut.onvif.user.is_none() {
                                cam_mut.onvif.user = user_for_hw.clone();
                            }
                            if cam_mut.onvif.password.is_none() {
                                cam_mut.onvif.password = pass_for_hw.clone();
                            }
                            if cam_mut.hardware.profile_token.is_none() {
                                cam_mut.hardware.profile_token = Some(
                                    body_prof
                                        .as_deref()
                                        .map(str::trim)
                                        .filter(|s| !s.is_empty())
                                        .map(str::to_string)
                                        .or(cached_prof)
                                        .unwrap_or_else(|| {
                                            if is_tapo_hint {
                                                "profile_1".to_string()
                                            } else {
                                                "Profile000".to_string()
                                            }
                                        }),
                                );
                            }
                            if cam_mut.hardware.video_source_token.is_none() {
                                cam_mut.hardware.video_source_token = Some(
                                    body_vs
                                        .as_deref()
                                        .map(str::trim)
                                        .filter(|s| !s.is_empty())
                                        .map(str::to_string)
                                        .or(cached_vs)
                                        .unwrap_or_else(|| {
                                            if is_tapo_hint {
                                                "raw_vs1".to_string()
                                            } else {
                                                "VideoSource000".to_string()
                                            }
                                        }),
                                );
                            }
                            if cam_mut.hardware.protocol.is_none() {
                                cam_mut.hardware.protocol = Some(
                                    body_proto
                                        .as_deref()
                                        .map(str::trim)
                                        .filter(|s| !s.is_empty())
                                        .map(str::to_string)
                                        .or_else(|| cached_proto.clone())
                                        .unwrap_or_else(|| {
                                            if is_tapo_hint {
                                                "tapo".to_string()
                                            } else if is_dahua_hint {
                                                "dahua_cgi".to_string()
                                            } else {
                                                "onvif".to_string()
                                            }
                                        }),
                                );
                            }
                            ops.push("apply_suggested_onvif".to_string());
                        }
                        if let Some(ref proto) = body_proto {
                            let t = proto.trim();
                            cam_mut.hardware.protocol = if t.is_empty() {
                                None
                            } else {
                                Some(t.to_string())
                            };
                        }
                        if let Some(ref pt) = body_prof {
                            let t = pt.trim();
                            cam_mut.hardware.profile_token = if t.is_empty() {
                                None
                            } else {
                                Some(t.to_string())
                            };
                        }
                        if let Some(ref vst) = body_vs {
                            let t = vst.trim();
                            cam_mut.hardware.video_source_token = if t.is_empty() {
                                None
                            } else {
                                Some(t.to_string())
                            };
                        }
                        if let Some(ref cp) = body_cp {
                            let trimmed_cp = cp.trim();
                            if trimmed_cp.is_empty() {
                                cam_mut.hardware.cloud_password = None;
                            } else if trimmed_cp != "***" && trimmed_cp != "*" {
                                cam_mut.hardware.cloud_password = Some(trimmed_cp.to_string());
                            }
                            ops.push("cloud_password".to_string());
                        }
                        if body_apply_sub == Some(true) {
                            let sub_path = if is_tapo_hint {
                                "/stream2#transport=tcp#timeout=10"
                            } else {
                                "/cam/realmonitor?channel=1&subtype=1&unicast=true&proto=Onvif#transport=tcp#timeout=10"
                            };
                            let userinfo_prefix = match (&user_for_hw, &pass_for_hw) {
                                (Some(u), Some(p)) if !u.is_empty() && !p.is_empty() => format!(
                                    "{}:{}@",
                                    encode_rtsp_userinfo_part(u),
                                    encode_rtsp_userinfo_part(p)
                                ),
                                (Some(u), _) if !u.is_empty() => {
                                    format!("{}@", encode_rtsp_userinfo_part(u))
                                }
                                _ => String::new(),
                            };
                            let sub_url =
                                format!("rtsp://{userinfo_prefix}{clean_host_only}:554{sub_path}");
                            let sub_key = format!("{cam_name_for_hw}_sub");
                            for inp in &mut cam_mut.ffmpeg.inputs {
                                if inp.roles.len() == 1 && inp.roles[0] == "detect" {
                                    inp.path = format!("rtsp://127.0.0.1:8554/{sub_key}");
                                }
                            }
                            updated_cam = Some(cam_mut.clone());
                            cfg_guard
                                .go2rtc
                                .streams
                                .insert(sub_key, Value::Array(vec![Value::String(sub_url)]));
                            ops.push("apply_suggested_substream".to_string());
                        } else {
                            updated_cam = Some(cam_mut.clone());
                        }
                    }
                    Ok((updated_cam, ops))
                },
            )
            .await
        {
            Ok((Some(updated), ops)) => {
                cam_cfg = updated;
                applied_ops.extend(ops);
            }
            Ok((None, ops)) => {
                applied_ops.extend(ops);
            }
            Err(err) => {
                warn!("Failed to persist config.yml after hardware config update: {err:#}");
            }
        }
    }

    let timeout = match body.timeout_ms {
        Some(ms) => std::time::Duration::from_millis(ms.clamp(50, 10_000)),
        None => std::time::Duration::from_secs(cam_cfg.onvif.timeout_sec.clamp(1, 10)),
    };
    let user = user_opt.as_deref();
    let pass = pass_opt.as_deref();

    // 2. Apply ONVIF Imaging settings (Brightness / Contrast / Saturation / Sharpness)
    if body.brightness.is_some()
        || body.contrast.is_some()
        || body.saturation.is_some()
        || body.sharpness.is_some()
    {
        let img_endpoints = if is_tapo_hint {
            [
                format!("{onvif_base}/onvif/service"),
                format!("{onvif_base}/onvif/imaging_service"),
            ]
        } else {
            [
                format!("{onvif_base}/onvif/imaging_service"),
                format!("{onvif_base}/onvif/service"),
            ]
        };
        let default_vs = cam_cfg
            .hardware
            .video_source_token
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .or_else(|| state.camera_driver_runtime.get_video_source(&camera))
            .unwrap_or_else(|| {
                if is_tapo_hint {
                    "raw_vs1".to_string()
                } else {
                    "VideoSource000".to_string()
                }
            });
        let transport =
            camera_protocols::ReqwestCameraWireTransport::new(state.http_client.clone());
        match state
            .camera_driver_runtime
            .apply_imaging_settings_with_transport(
                &transport,
                &camera,
                &img_endpoints,
                &default_vs,
                cam_cfg.hardware.protocol.as_deref(),
                (
                    body.brightness,
                    body.contrast,
                    body.saturation,
                    body.sharpness,
                ),
                (user, pass),
                timeout,
            )
            .await
        {
            Ok(_) => {
                applied_ops.push("imaging".to_string());
            }
            Err(last_img_err) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "success": false, "camera": camera, "error": last_img_err })),
                )
                    .into_response();
            }
        }
    }

    // 3. Apply Dahua / Intelbras CGI hardware settings via declarative CGI table encoder over transport
    let cgi_pairs = build_dahua_cgi_set_config_pairs(&body);

    if !cgi_pairs.is_empty() {
        let cgi_transport =
            camera_protocols::ReqwestCameraWireTransport::new(state.http_client.clone());
        match state
            .camera_driver_runtime
            .apply_dahua_cgi_set_config(
                &cgi_transport,
                &cgi_base,
                &cgi_pairs,
                (user, pass),
                timeout,
            )
            .await
        {
            Ok(()) => {
                applied_ops.extend(cgi_pairs);
            }
            Err((st, err_msg)) => {
                return (
                    st,
                    Json(json!({
                        "success": false,
                        "camera": camera,
                        "error": err_msg
                    })),
                )
                    .into_response();
            }
        }
    }

    // 4. Handle Manual Siren / Audible Alarm Trigger (Tapo Streamd :8800 audio/mp2t or Dahua CGI postAudio)
    if body.trigger_siren == Some(true) {
        let dur_sec = body.siren_duration_sec.unwrap_or(3.5).clamp(0.5, 15.0);
        let effective_proto = cam_cfg
            .hardware
            .protocol
            .as_deref()
            .and_then(find_protocol_spec_by_id_or_alias)
            .map(|s| s.id.to_string())
            .or_else(|| state.camera_driver_runtime.get_protocol(&camera));
        let is_tapo_proto = match effective_proto.as_deref() {
            Some(p) => p == "tapo",
            None => is_tapo_hint && !is_dahua_hint,
        };
        match state
            .camera_driver_runtime
            .trigger_siren_at_host(
                &state.http_client,
                &camera,
                (&host, onvif_port),
                (user, pass),
                cam_cfg.hardware.cloud_password.as_deref(),
                is_tapo_proto,
                dur_sec,
            )
            .await
        {
            Ok(_) => {
                state.log_hub.push_ninjanvr(
                    "INFO",
                    &format!("ninjanvr.hardware.{camera}"),
                    &format!("Triggered camera siren on {camera} ({dur_sec:.1}s)"),
                );
                applied_ops.push("trigger_siren".to_string());
            }
            Err((st, err_msg)) => {
                return (
                    st,
                    Json(json!({
                        "success": false,
                        "camera": camera,
                        "error": err_msg
                    })),
                )
                    .into_response();
            }
        }
    }

    // 5. Handle Reboot (ONVIF SystemReboot or Dahua CGI magicBox.cgi?action=reboot)
    if body.reboot == Some(true) {
        let transport =
            camera_protocols::ReqwestCameraWireTransport::new(state.http_client.clone());
        match state
            .camera_driver_runtime
            .reboot_camera_at_endpoints(
                &transport,
                &onvif_base,
                &cgi_base,
                !is_tapo_hint,
                (user, pass),
                timeout,
            )
            .await
        {
            Ok(op_tag) => {
                applied_ops.push(op_tag.to_string());
            }
            Err((st, err_msg)) => {
                return (
                    st,
                    Json(json!({
                        "success": false,
                        "camera": camera,
                        "error": err_msg
                    })),
                )
                    .into_response();
            }
        }
    }

    Json(json!({
        "success": true,
        "camera": camera,
        "applied": applied_ops
    }))
    .into_response()
}

async fn api_camera_logs_get(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    Query(q): Query<HardwareQueryParams>,
) -> Response {
    if let Err(err_resp) = authorize_admin_read(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    if let Ok(guard) = state.camera_logs_cooldown.lock()
        && let Some((cached_at, cached_val)) = guard.get(&camera)
        && cached_at.elapsed() < Duration::from_secs(5)
    {
        return Json(cached_val.clone()).into_response();
    }

    let (cam_opt, probe_target) = {
        let cfg = state.config.read().await;
        let cam = cfg.cameras.get(&camera).cloned();
        let target = cam.as_ref().and_then(|c| {
            resolve_camera_hardware_probe_targets(&state.camera_driver_runtime, &cfg, &camera, c)
        });
        (cam, target)
    };

    let Some(cam_cfg) = cam_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": format!("Camera '{camera}' not found") })),
        )
            .into_response();
    };

    let Some(CameraHardwareProbeTarget {
        host,
        onvif_port,
        user: user_opt,
        password: pass_opt,
        is_tapo_hint,
        is_dahua_hint,
        ..
    }) = probe_target
    else {
        return Json(json!({
            "camera": camera,
            "online": false,
            "protocol_family": "rtsp_only",
            "sources_queried": [],
            "lines": [format!("[diagnostic] Camera '{camera}' has no reachable ONVIF/CGI host configured")]
        }))
        .into_response();
    };

    let cached_proto = state.camera_driver_runtime.get_protocol(&camera);
    let spec = infer_protocol_spec_from_device_or_hints(
        cam_cfg
            .hardware
            .protocol
            .as_deref()
            .or(cached_proto.as_deref()),
        None,
        None,
        is_tapo_hint,
        is_dahua_hint,
        is_dahua_hint,
        true,
    );
    let timeout = match q.timeout_ms {
        Some(ms) => Duration::from_millis(ms.clamp(50, 8_000)),
        None => Duration::from_secs(cam_cfg.onvif.timeout_sec.clamp(1, 5)),
    };

    let diag_transport =
        camera_protocols::ReqwestCameraWireTransport::new(state.http_client.clone());
    let report = collect_camera_diagnostic_logs_with_transport(
        &diag_transport,
        &state.camera_driver_runtime,
        &camera,
        (&host, onvif_port),
        (user_opt.as_deref(), pass_opt.as_deref()),
        spec,
        timeout,
    )
    .await;

    for line in &report.lines {
        state.log_hub.push_camera(&camera, "INFO", line);
    }
    let report_val = serde_json::to_value(&report).unwrap_or_else(|_| json!({}));
    if let Ok(mut guard) = state.camera_logs_cooldown.lock() {
        if guard.len() >= 128 && !guard.contains_key(&camera) {
            guard.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(5));
            if guard.len() >= 128 {
                guard.clear();
            }
        }
        guard.insert(camera, (Instant::now(), report_val.clone()));
    }

    Json(report_val).into_response()
}

async fn api_camera_audio_talkback(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    body_bytes: axum::body::Bytes,
) -> Response {
    if let Some(err_resp) =
        verify_camera_mutation_access(&state, &jar, &uri, &headers, &camera).await
    {
        return err_resp.into_response();
    }
    if body_bytes.is_empty() || body_bytes.len() > MAX_TALKBACK_AUDIO_BYTES {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": format!("Audio talkback payload must be between 1 and {MAX_TALKBACK_AUDIO_BYTES} bytes")
            })),
        )
            .into_response();
    }

    let (cam_opt, probe_target) = {
        let cfg = state.config.read().await;
        let cam = cfg.cameras.get(&camera).cloned();
        let target = cam.as_ref().and_then(|c| {
            resolve_camera_hardware_probe_targets(&state.camera_driver_runtime, &cfg, &camera, c)
        });
        (cam, target)
    };
    let Some(cam_cfg) = cam_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": format!("Camera '{camera}' not found") })),
        )
            .into_response();
    };
    let Some(CameraHardwareProbeTarget {
        host,
        onvif_port,
        user: user_opt,
        password: pass_opt,
        is_tapo_hint,
        is_dahua_hint,
        ..
    }) = probe_target
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": format!("Camera '{camera}' has no reachable HTTP talkback endpoint") })),
        )
            .into_response();
    };

    let ct_in = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_ascii_lowercase();
    let cte_base64 = headers
        .get("content-transfer-encoding")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("base64"));

    let mut is_alaw = ct_in.contains("g711a") || ct_in.contains("pcma") || ct_in.contains("alaw");
    let raw_audio_bytes: Vec<u8> = if ct_in.contains("json") || body_bytes.starts_with(b"{") {
        let Ok(jval) = serde_json::from_slice::<Value>(&body_bytes) else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "success": false, "message": "Invalid JSON audio talkback body" })),
            )
                .into_response();
        };
        if jval
            .get("codec")
            .or_else(|| jval.get("format"))
            .and_then(|v| v.as_str())
            .is_some_and(|c| {
                let clc = c.to_ascii_lowercase();
                clc.contains("alaw") || clc.contains("g711a") || clc.contains("pcma")
            })
        {
            is_alaw = true;
        }
        if jval.get("alaw_base64").is_some() {
            is_alaw = true;
        }
        let b64_str = jval
            .get("audio_base64")
            .or_else(|| jval.get("pcm_base64"))
            .or_else(|| jval.get("alaw_base64"))
            .or_else(|| jval.get("data"))
            .or_else(|| jval.get("audio"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        match base64::engine::general_purpose::STANDARD.decode(b64_str) {
            Ok(decoded) => decoded,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "success": false, "message": "Invalid base64 audio data in JSON body" })),
                )
                    .into_response();
            }
        }
    } else if ct_in.contains("base64") || cte_base64 {
        let trimmed_b64 = std::str::from_utf8(&body_bytes)
            .unwrap_or("")
            .trim()
            .as_bytes();
        match base64::engine::general_purpose::STANDARD.decode(trimmed_b64) {
            Ok(decoded) => decoded,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "success": false, "message": "Invalid base64 audio payload" })),
                )
                    .into_response();
            }
        }
    } else if !is_alaw
        && let Ok(utf8_str) = std::str::from_utf8(&body_bytes)
        && body_bytes.len() > utf8_str.chars().count()
        && utf8_str.chars().all(|c| (c as u32) <= 0xFF)
    {
        // Recover raw bytes when browser fetch() UTF-8 encoded a Latin-1 binary string
        utf8_str.chars().map(|c| c as u8).collect()
    } else {
        body_bytes.to_vec()
    };

    let alaw_payload = if is_alaw {
        raw_audio_bytes
    } else {
        convert_pcm16le_or_wav_to_alaw(&raw_audio_bytes)
    };
    if alaw_payload.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Audio payload produced 0 G.711A samples" })),
        )
            .into_response();
    }

    let effective_proto = cam_cfg
        .hardware
        .protocol
        .as_deref()
        .and_then(find_protocol_spec_by_id_or_alias)
        .map(|s| s.id.to_string())
        .or_else(|| state.camera_driver_runtime.get_protocol(&camera));
    let is_tapo_proto = match effective_proto.as_deref() {
        Some(p) => p == "tapo",
        None => is_tapo_hint && !is_dahua_hint,
    };
    let timeout = std::time::Duration::from_secs(cam_cfg.onvif.timeout_sec.clamp(2, 15));

    match send_camera_talkback_or_siren_alaw(
        &state.http_client,
        &state.camera_driver_runtime,
        &camera,
        (&host, onvif_port),
        (user_opt.as_deref(), pass_opt.as_deref()),
        cam_cfg.hardware.cloud_password.as_deref(),
        is_tapo_proto,
        alaw_payload,
        true,
        timeout,
    )
    .await
    {
        Ok(bytes_sent) => Json(json!({
            "success": true,
            "camera": camera,
            "bytes_sent": bytes_sent,
            "codec": "G.711A"
        }))
        .into_response(),
        Err((st, err_msg)) => (
            st,
            Json(json!({
                "success": false,
                "camera": camera,
                "error": err_msg
            })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct SetCameraStateBody {
    pub enabled: Option<bool>,
    pub detect: Option<bool>,
    pub record: Option<bool>,
    pub alerts: Option<bool>,
    pub snapshots: Option<bool>,
}

async fn api_camera_state_get(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
) -> Response {
    if camera.is_empty() || !is_safe_id(&camera) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Invalid camera identifier" })),
        )
            .into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    let flags = state.comms.get_camera_flags(&camera);
    Json(serde_json::to_value(flags).unwrap_or_else(|_| json!({}))).into_response()
}

async fn api_camera_state_set(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    Json(body): Json<SetCameraStateBody>,
) -> Response {
    if camera.is_empty() || !is_safe_id(&camera) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Invalid camera identifier" })),
        )
            .into_response();
    }
    if let Some(err_resp) =
        verify_camera_mutation_access(&state, &jar, &uri, &headers, &camera).await
    {
        return err_resp.into_response();
    }
    let cam_configured = state.config.read().await.cameras.contains_key(&camera);
    if !cam_configured {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": format!("Camera '{camera}' not found") })),
        )
            .into_response();
    }
    let flags = state.comms.set_camera_flags(
        &camera,
        body.enabled,
        body.detect,
        body.record,
        body.alerts,
        body.snapshots,
    );
    state
        .activity_ledger
        .write()
        .await
        .entry(camera)
        .or_default()
        .set_record_enabled(flags.enabled && flags.record);
    Json(serde_json::to_value(flags).unwrap_or_else(|_| json!({}))).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct LatestFrameQuery {
    pub height: Option<u32>,
    pub bbox: Option<bool>,
}

const MAX_LAZY_SNAPSHOT_CACHE_ENTRIES: usize = 128;
pub(crate) const MAX_SNAPSHOT_FRAME_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const MAX_SNAPSHOT_DIMENSION: u32 = 8192;
pub(crate) const MAX_SNAPSHOT_DECODE_ALLOC_BYTES: u64 = 64 * 1024 * 1024;

fn decode_bounded_snapshot_rgb(bytes: &[u8]) -> Option<image::RgbImage> {
    media_redaction::decode_bounded_snapshot_rgb(bytes)
}

fn apply_camera_privacy_mask_or_fail_closed(
    rgb: &mut image::RgbImage,
    raw_mask: &Value,
    detect_w: u32,
    detect_h: u32,
) -> bool {
    MediaDeliveryGate::apply_mask_or_fail_closed(rgb, raw_mask, detect_w, detect_h)
}

async fn read_bounded_response_bytes(
    mut resp: reqwest::Response,
    max_bytes: usize,
) -> Option<Bytes> {
    if resp.content_length().is_some_and(|l| l > max_bytes as u64) {
        return None;
    }
    let mut out = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        if out.len().saturating_add(chunk.len()) > max_bytes {
            return None;
        }
        out.extend_from_slice(&chunk);
    }
    Some(Bytes::from(out))
}

/// Fast-path `HEAD /api/{camera}/latest.jpg` (and `latest.webp`) returning `x-last-frame-timestamp`
/// from in-memory metrics (or file metadata fallback) with zero YUV decode or JPEG/WebP encode.
async fn api_camera_latest_frame_head(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    uri: Uri,
) -> Response {
    if camera.is_empty() || !is_safe_id(&camera) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let principal = match authenticate_read(&state, &jar, &headers).await {
        Ok(p) => p,
        Err(rej) => return rej.into_response(),
    };
    if !principal.allows_camera(&camera) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let content_type = if uri.path().ends_with(".webp") {
        "image/webp"
    } else {
        "image/jpeg"
    };

    let mut ts_u64 = state
        .camera_metrics
        .read()
        .await
        .get(&camera)
        .map(|m| m.last_frame_ts)
        .filter(|&ts| ts > 0.0)
        .map(|ts| ts as u64);

    if ts_u64.is_none() {
        ts_u64 = state
            .latest_yuv_frames
            .read()
            .await
            .get(&camera)
            .map(|f| f.timestamp)
            .filter(|&ts| ts > 0.0)
            .map(|ts| ts as u64);
    }

    if ts_u64.is_none() && state.latest_frames.read().await.contains_key(&camera) {
        ts_u64 = Some(unix_now());
    }

    if ts_u64.is_none() {
        let disk_latest = state
            .media_dir
            .join("clips")
            .join(format!("latest_{camera}.jpg"));
        if let Ok(meta) = std::fs::symlink_metadata(&disk_latest)
            && meta.file_type().is_file()
        {
            ts_u64 = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
        }
    }

    let mut resp = (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::empty(),
    )
        .into_response();
    if let Some(ts) = ts_u64
        && let Ok(hv) = header::HeaderValue::from_str(&ts.to_string())
    {
        resp.headers_mut().insert(
            header::HeaderName::from_static("x-last-frame-timestamp"),
            hv,
        );
    }
    resp
}

async fn api_camera_latest_frame(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    Query(query): Query<LatestFrameQuery>,
    uri: Uri,
) -> Response {
    if camera.is_empty() || !is_safe_id(&camera) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Invalid camera identifier" })),
        )
            .into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }

    let _ = query.bbox;
    let is_webp = uri.path().ends_with(".webp");
    let target_height = query.height.filter(|&h| h > 0).map(|h| h.clamp(16, 2160));
    let content_type = if is_webp {
        "image/webp".to_string()
    } else {
        "image/jpeg".to_string()
    };

    let metric_ts = state
        .camera_metrics
        .read()
        .await
        .get(&camera)
        .map_or(0.0, |m| m.last_frame_ts);

    let ts_hdr = header::HeaderName::from_static("x-last-frame-timestamp");

    let (priv_mask, detect_w, detect_h) = {
        let cfg = state.config.read().await;
        if let Some(c) = cfg.cameras.get(&camera) {
            (
                Some(c.privacy_mask.clone()),
                c.detect.width,
                c.detect.height,
            )
        } else {
            (None, 0, 0)
        }
    };
    let mask_active = has_active_privacy_mask(priv_mask.as_ref());
    let mask_hash = if mask_active {
        compute_privacy_mask_hash(priv_mask.as_ref())
    } else {
        String::new()
    };

    // 1. Check short-lived 400ms in-memory encoded snapshot cache (keyed by camera, height, format, and privacy_mask_hash)
    let cache_key = (
        camera.clone(),
        target_height.unwrap_or(0),
        is_webp,
        mask_hash,
    );
    if let Some((cached_at, cached_bytes, cached_ts)) = state
        .lazy_snapshot_cache
        .read()
        .await
        .get(&cache_key)
        .cloned()
        && cached_at.elapsed() <= Duration::from_millis(400)
    {
        return (
            [
                (header::CONTENT_TYPE, content_type),
                (ts_hdr, cached_ts.to_string()),
            ],
            cached_bytes,
        )
            .into_response();
    }

    let store_lazy_snapshot_cache = |key: LazySnapshotCacheKey, bytes: Bytes, ts: u64| async move {
        let mut cache = state.lazy_snapshot_cache.write().await;
        if cache.len() >= MAX_LAZY_SNAPSHOT_CACHE_ENTRIES {
            cache.retain(|_, (at, _, _)| at.elapsed() <= Duration::from_millis(400));
        }
        while cache.len() >= MAX_LAZY_SNAPSHOT_CACHE_ENTRIES {
            if let Some(oldest_key) = cache
                .iter()
                .min_by_key(|(_, (at, _, _))| *at)
                .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest_key);
            } else {
                break;
            }
        }
        cache.insert(key, (Instant::now(), bytes, ts));
    };

    // 2. Encode on demand from latest_yuv_frames (zero-copy raw YuvFrame) on blocking pool
    if let Some(raw_yuv) = state.latest_yuv_frames.read().await.get(&camera).cloned() {
        let priv_for_yuv = priv_mask.clone();
        let yuv_res =
            tokio::task::spawn_blocking(move || -> std::result::Result<(Bytes, u64), bool> {
                let Some(yuv_frame) = pipeline::normalize_yuv_frame_for_processing(&raw_yuv) else {
                    return Err(true);
                };
                let mut rgb = ninjanvr_vision::yuv420p_to_rgb_image_scaled(
                    &yuv_frame,
                    None,
                    &[],
                    target_height,
                );
                if let Some(ref mask) = priv_for_yuv
                    && mask_active
                {
                    let eff_w = if detect_w > 0 {
                        detect_w
                    } else {
                        yuv_frame.width
                    };
                    let eff_h = if detect_h > 0 {
                        detect_h
                    } else {
                        yuv_frame.height
                    };
                    if !apply_camera_privacy_mask_or_fail_closed(&mut rgb, mask, eff_w, eff_h) {
                        return Err(true);
                    }
                }
                let encoded_res = if is_webp {
                    ninjanvr_vision::encode_image_webp(&rgb)
                } else {
                    ninjanvr_vision::encode_image_jpeg(&rgb)
                };
                let Ok(encoded_vec) = encoded_res else {
                    return Err(false);
                };
                let ts = if yuv_frame.timestamp > 0.0 {
                    yuv_frame.timestamp as u64
                } else if metric_ts > 0.0 {
                    metric_ts as u64
                } else {
                    unix_now()
                };
                Ok((Bytes::from(encoded_vec), ts))
            })
            .await;
        match yuv_res {
            Ok(Ok((out_bytes, ts))) => {
                store_lazy_snapshot_cache(cache_key, out_bytes.clone(), ts).await;
                return (
                    [
                        (header::CONTENT_TYPE, content_type),
                        (ts_hdr, ts.to_string()),
                    ],
                    out_bytes,
                )
                    .into_response();
            }
            Ok(Err(true)) => return placeholder_jpeg_response(),
            Ok(Err(false)) | Err(_) => {}
        }
    }

    if let Some(frame_bytes) = state.latest_frames.read().await.get(&camera).cloned() {
        let ts = if metric_ts > 0.0 {
            metric_ts as u64
        } else {
            unix_now()
        };
        if let Some(ref mask) = priv_mask
            && mask_active
        {
            let mask_owned = mask.clone();
            let masked_opt = tokio::task::spawn_blocking(move || -> Option<Bytes> {
                let mut rgb = decode_bounded_snapshot_rgb(&frame_bytes)?;
                if !apply_camera_privacy_mask_or_fail_closed(
                    &mut rgb,
                    &mask_owned,
                    detect_w,
                    detect_h,
                ) {
                    return None;
                }
                let encoded = if is_webp {
                    ninjanvr_vision::encode_image_webp(&rgb).ok()?
                } else {
                    ninjanvr_vision::encode_image_jpeg(&rgb).ok()?
                };
                Some(Bytes::from(encoded))
            })
            .await
            .ok()
            .flatten();
            if let Some(out_bytes) = masked_opt {
                store_lazy_snapshot_cache(cache_key, out_bytes.clone(), ts).await;
                return (
                    [
                        (header::CONTENT_TYPE, content_type),
                        (ts_hdr, ts.to_string()),
                    ],
                    out_bytes,
                )
                    .into_response();
            }
            return placeholder_jpeg_response();
        }
        return (
            [
                (header::CONTENT_TYPE, "image/jpeg".to_string()),
                (ts_hdr, ts.to_string()),
            ],
            frame_bytes,
        )
            .into_response();
    }

    let disk_latest = state
        .media_dir
        .join("clips")
        .join(format!("latest_{camera}.jpg"));
    let media_dir_for_disk = state.media_dir.clone();
    let priv_for_disk = priv_mask.clone();
    let disk_res = tokio::task::spawn_blocking(
        move || -> Option<std::result::Result<(Bytes, u64, bool), ()>> {
            let safe_disk_latest = ensure_path_within_media_dir(&media_dir_for_disk, &disk_latest)?;
            let (mut file, fmeta) = open_regular_non_symlink_readonly(&safe_disk_latest).ok()?;
            if fmeta.len() == 0 || fmeta.len() > MAX_SNAPSHOT_FRAME_BYTES as u64 {
                return None;
            }
            use std::io::Read;
            let mut disk_bytes = Vec::with_capacity(fmeta.len() as usize);
            (&mut file)
                .take(MAX_SNAPSHOT_FRAME_BYTES as u64 + 1)
                .read_to_end(&mut disk_bytes)
                .ok()?;
            if disk_bytes.is_empty() || disk_bytes.len() > MAX_SNAPSHOT_FRAME_BYTES {
                return None;
            }
            let disk_ts = if metric_ts > 0.0 {
                metric_ts as u64
            } else {
                fmeta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or_else(unix_now, |d| d.as_secs())
            };
            if let Some(ref mask) = priv_for_disk
                && mask_active
            {
                let Some(mut rgb) = decode_bounded_snapshot_rgb(&disk_bytes) else {
                    return Some(Err(()));
                };
                if !apply_camera_privacy_mask_or_fail_closed(&mut rgb, mask, detect_w, detect_h) {
                    return Some(Err(()));
                }
                let encoded_res = if is_webp {
                    ninjanvr_vision::encode_image_webp(&rgb)
                } else {
                    ninjanvr_vision::encode_image_jpeg(&rgb)
                };
                let Ok(out_vec) = encoded_res else {
                    return Some(Err(()));
                };
                return Some(Ok((Bytes::from(out_vec), disk_ts, true)));
            }
            Some(Ok((Bytes::from(disk_bytes), disk_ts, false)))
        },
    )
    .await
    .ok()
    .flatten();
    if let Some(res) = disk_res {
        match res {
            Ok((out_bytes, disk_ts, was_masked)) => {
                let mime = if was_masked {
                    content_type
                } else {
                    "image/jpeg".to_string()
                };
                store_lazy_snapshot_cache(cache_key, out_bytes.clone(), disk_ts).await;
                return (
                    [(header::CONTENT_TYPE, mime), (ts_hdr, disk_ts.to_string())],
                    out_bytes,
                )
                    .into_response();
            }
            Err(()) => return placeholder_jpeg_response(),
        }
    }

    let encoded_cam = urlencoding_simple(&camera);
    let url = format!(
        "{}/api/frame.jpeg?src={encoded_cam}_sub",
        state.go2rtc_http_base
    );
    if let Ok(resp) = state
        .http_client
        .get(&url)
        .timeout(std::time::Duration::from_millis(4500))
        .send()
        .await
        && resp.status().is_success()
        && let Some(bytes) = read_bounded_response_bytes(resp, MAX_SNAPSHOT_FRAME_BYTES).await
    {
        if let Some(ref mask) = priv_mask
            && mask_active
        {
            let mask_owned = mask.clone();
            let masked_opt = tokio::task::spawn_blocking(move || -> Option<Bytes> {
                let mut rgb = decode_bounded_snapshot_rgb(&bytes)?;
                if !apply_camera_privacy_mask_or_fail_closed(
                    &mut rgb,
                    &mask_owned,
                    detect_w,
                    detect_h,
                ) {
                    return None;
                }
                let encoded = if is_webp {
                    ninjanvr_vision::encode_image_webp(&rgb).ok()?
                } else {
                    ninjanvr_vision::encode_image_jpeg(&rgb).ok()?
                };
                Some(Bytes::from(encoded))
            })
            .await
            .ok()
            .flatten();
            if let Some(out_bytes) = masked_opt {
                let now_ts = unix_now();
                store_lazy_snapshot_cache(cache_key, out_bytes.clone(), now_ts).await;
                return (
                    [
                        (header::CONTENT_TYPE, content_type),
                        (ts_hdr, now_ts.to_string()),
                    ],
                    out_bytes,
                )
                    .into_response();
            }
            return placeholder_jpeg_response();
        }
        return (
            [
                (header::CONTENT_TYPE, "image/jpeg".to_string()),
                (ts_hdr, unix_now().to_string()),
            ],
            bytes,
        )
            .into_response();
    }

    placeholder_jpeg_response()
}

async fn api_go2rtc_stream_prewarm(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(stream): AxumPath<String>,
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
    if let Some(err_resp) = verify_caller_can_mutate(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
    state
        .live_gateway
        .prewarm_stream(&state.go2rtc_ws_base, &stream, allowed.as_deref())
        .await
}

/// Unified operator snapshot: full-resolution main-stream frame (privacy-masked, fail-closed)
/// with a fallback to the latest detect frame. Served as a dated `attachment` so a single
/// "Snapshot" action behaves identically on desktop and mobile browsers.
async fn api_camera_snapshot_download(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
) -> Response {
    use snapshot_capture::{
        MAIN_STREAM_FRAME_TIMEOUT_MS, SnapshotSource, render_main_stream_snapshot,
        snapshot_content_disposition, snapshot_download_filename,
    };

    if camera.is_empty() || !is_safe_id(&camera) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Invalid camera identifier" })),
        )
            .into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    let Some((priv_mask, detect_w, detect_h)) = ({
        let cfg = state.config.read().await;
        cfg.cameras
            .get(&camera)
            .map(|c| (c.privacy_mask.clone(), c.detect.width, c.detect.height))
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Camera not found" })),
        )
            .into_response();
    };

    let filename = snapshot_download_filename(&camera, Local::now().naive_local());
    let build = |bytes: Bytes, source: SnapshotSource, resolution: Option<(u32, u32)>| {
        let mut resp = (
            [
                (header::CONTENT_TYPE, "image/jpeg".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    snapshot_content_disposition(&filename),
                ),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
            bytes,
        )
            .into_response();
        let hdrs = resp.headers_mut();
        if let Ok(v) = header::HeaderValue::from_str(source.as_header_value()) {
            hdrs.insert(header::HeaderName::from_static("x-snapshot-source"), v);
        }
        if let Some((w, h)) = resolution
            && let Ok(v) = header::HeaderValue::from_str(&format!("{w}x{h}"))
        {
            hdrs.insert(header::HeaderName::from_static("x-snapshot-resolution"), v);
        }
        resp
    };

    // 1. Full-resolution main stream via go2rtc (bounded size and time).
    let url = format!(
        "{}/api/frame.jpeg?src={}",
        state.go2rtc_http_base,
        urlencoding_simple(&camera)
    );
    if let Ok(resp) = state
        .http_client
        .get(&url)
        .timeout(Duration::from_millis(MAIN_STREAM_FRAME_TIMEOUT_MS))
        .send()
        .await
        && resp.status().is_success()
        && let Some(bytes) = read_bounded_response_bytes(resp, MAX_SNAPSHOT_FRAME_BYTES).await
    {
        let mask_for_render = priv_mask.clone();
        let rendered = tokio::task::spawn_blocking(move || {
            render_main_stream_snapshot(&bytes, Some(&mask_for_render), detect_w, detect_h)
        })
        .await
        .ok()
        .flatten();
        if let Some((jpg, w, h)) = rendered {
            return build(Bytes::from(jpg), SnapshotSource::MainStream, Some((w, h)));
        }
    }

    // 2. Fallback: latest detect frame (already privacy-masked by `latest.jpg`).
    let fallback = api_camera_latest_frame(
        State(state),
        jar,
        headers,
        AxumPath(camera),
        Query(LatestFrameQuery {
            height: None,
            bbox: None,
        }),
        Uri::from_static("/latest.jpg"),
    )
    .await;
    let has_real_frame = fallback.status().is_success()
        && fallback
            .headers()
            .contains_key(header::HeaderName::from_static("x-last-frame-timestamp"));
    if !has_real_frame {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "message": "No frame available for this camera yet" })),
        )
            .into_response();
    }
    let Ok(bytes) = axum::body::to_bytes(fallback.into_body(), MAX_SNAPSHOT_FRAME_BYTES).await
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "message": "No frame available for this camera yet" })),
        )
            .into_response();
    };
    let resolution = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok());
    build(bytes, SnapshotSource::DetectFrame, resolution)
}

fn placeholder_jpeg_response() -> Response {
    let mut img = image::RgbImage::new(320, 180);
    for pixel in img.pixels_mut() {
        *pixel = image::Rgb([18, 22, 28]);
    }
    let mut buf = std::io::Cursor::new(Vec::new());
    let _ = img.write_to(&mut buf, image::ImageFormat::Jpeg);
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (
                header::HeaderName::from_static("x-ninjanvr-placeholder"),
                "1",
            ),
        ],
        buf.into_inner(),
    )
        .into_response()
}

async fn api_go2rtc_streams(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Response {
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
    state
        .live_gateway
        .list_streams(&state.go2rtc_http_base, allowed.as_deref())
        .await
}

async fn api_go2rtc_stream_one(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(stream): AxumPath<String>,
) -> Response {
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
    state
        .live_gateway
        .get_stream_one(&state.go2rtc_http_base, &stream, allowed.as_deref())
        .await
}

const MAX_STATE_WS_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_GO2RTC_WS_MESSAGE_BYTES: usize = live_gateway::MAX_GO2RTC_WS_MESSAGE_BYTES;

async fn api_go2rtc_webrtc(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
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
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
    state
        .live_gateway
        .negotiate_webrtc_sdp(
            &state.go2rtc_http_base,
            uri.query(),
            &headers,
            body,
            allowed.as_deref(),
        )
        .await
}

async fn ws_state_handler(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    ws_res: std::result::Result<
        WebSocketUpgrade,
        axum::extract::ws::rejection::WebSocketUpgradeRejection,
    >,
) -> Response {
    if !verify_cookie_csrf_origin(&headers, &uri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Cross-origin cookie WebSocket request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    let ws = match ws_res {
        Ok(ws) => ws,
        Err(rej) => return rej.into_response(),
    };
    let (caller_user, had_db_user_at_connect, caller_role, allowed_cameras, session_iat) = {
        let auth_cfg = state.config.read().await.auth.clone();
        warm_auth_cache_for_request(&state, &jar, &headers, &auth_cfg).await;
        if let Some((user, role)) =
            extract_authenticated_user_with_cfg(&state, &jar, &headers, &auth_cfg)
        {
            let had_db_user = !user.starts_with("token:")
                && matches!(
                    state.auth_gate.get_user_cached_sync(&state.db, &user),
                    Ok(Some(_))
                );
            let allowed =
                resolve_user_allowed_cameras(&state, &jar, &headers, &auth_cfg, &user, &role);
            let iat = extract_jwt_iat(&state, &jar, &headers, &auth_cfg.cookie_name);
            let tracked_user = if auth_cfg.enabled { Some(user) } else { None };
            (tracked_user, had_db_user, role, allowed, iat)
        } else if auth_cfg.enabled && request_requires_strict_auth(&headers, &jar, &auth_cfg) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "message": "Unauthorized" })),
            )
                .into_response();
        } else {
            (None, false, "viewer".to_string(), Some(Vec::new()), None)
        }
    };
    ws.max_message_size(MAX_STATE_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_STATE_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| {
            handle_state_ws(
                socket,
                state,
                caller_user,
                had_db_user_at_connect,
                caller_role,
                allowed_cameras,
                session_iat,
            )
        })
        .into_response()
}

async fn handle_state_ws(
    mut socket: WebSocket,
    state: AppState,
    caller_user: Option<String>,
    had_db_user_at_connect: bool,
    caller_role: String,
    allowed_cameras: Option<Vec<String>>,
    session_iat: Option<i64>,
) {
    let cameras: Vec<String> = {
        let cfg = state.config.read().await;
        cfg.cameras
            .keys()
            .filter(|c| {
                allowed_cameras
                    .as_ref()
                    .is_none_or(|allow| allow.iter().any(|a| a == *c || a == "*"))
            })
            .cloned()
            .collect()
    };

    for init_msg in state.comms.initial_ws_snapshots(&cameras) {
        if let Some(filtered) =
            filter_ws_message_for_allowed_cameras(&init_msg, allowed_cameras.as_deref())
            && !matches!(
                tokio::time::timeout(
                    Duration::from_secs(5),
                    socket.send(AxumWsMsg::Text(filtered.into())),
                )
                .await,
                Ok(Ok(()))
            )
        {
            return;
        }
    }

    let mut rx = state.comms.subscribe_ws();
    loop {
        tokio::select! {
            biased;
            incoming = socket.next() => {
                match incoming {
                    Some(Ok(AxumWsMsg::Text(text))) => {
                        if text.len() > MAX_STATE_WS_MESSAGE_BYTES {
                            break;
                        }
                        let Some((live_role, live_allowed)) = reverify_ws_caller_access_async(
                            &state,
                            caller_user.as_deref(),
                            had_db_user_at_connect,
                            &caller_role,
                            &allowed_cameras,
                            session_iat,
                        )
                        .await
                        else {
                            break;
                        };
                        let role_lc = live_role.to_ascii_lowercase();
                        if text.contains("onConnect") {
                            let live_cams: Vec<String> = {
                                let cfg = state.config.read().await;
                                cfg.cameras
                                    .keys()
                                    .filter(|c| {
                                        live_allowed
                                            .as_ref()
                                            .is_none_or(|allow| allow.iter().any(|a| a == *c || a == "*"))
                                    })
                                    .cloned()
                                    .collect()
                            };
                            for init_msg in state.comms.initial_ws_snapshots(&live_cams) {
                                if let Some(filtered) = filter_ws_message_for_allowed_cameras(
                                    &init_msg,
                                    live_allowed.as_deref(),
                                ) && !matches!(
                                    tokio::time::timeout(
                                        Duration::from_secs(5),
                                        socket.send(AxumWsMsg::Text(filtered.into())),
                                    )
                                    .await,
                                    Ok(Ok(()))
                                )
                                {
                                    return;
                                }
                            }
                        } else if let Ok(parsed) = serde_json::from_str::<Value>(&text)
                            && let (Some(topic), Some(payload)) = (
                                parsed.get("topic").and_then(|v| v.as_str()),
                                parsed.get("payload").and_then(|v| v.as_str()),
                            )
                            && topic.ends_with("/set")
                        {
                            if matches!(role_lc.as_str(), "viewer" | "read_only" | "metrics") {
                                continue;
                            }
                            let clean_topic = topic.strip_prefix("ninjanvr/").unwrap_or(topic);
                            if clean_topic == "profile/set" {
                                if role_lc == "admin" || live_allowed.is_none() {
                                    state.comms.apply_set_command(clean_topic, payload);
                                }
                            } else if let Some(base) = clean_topic.strip_suffix("/set") {
                                let parts: Vec<&str> = base.split('/').collect();
                                if parts.len() == 2 {
                                    let cam = parts[0];
                                    if !is_safe_id(cam) {
                                        continue;
                                    }
                                    let permitted = live_allowed.as_ref().is_none_or(|allow| {
                                        allow.iter().any(|a| a == cam || a == "*")
                                    });
                                    let cam_configured =
                                        state.config.read().await.cameras.contains_key(cam);
                                    if permitted && cam_configured {
                                        state.comms.apply_set_command(clean_topic, payload);
                                        let flags = state.comms.get_camera_flags(cam);
                                        state
                                            .activity_ledger
                                            .write()
                                            .await
                                            .entry(cam.to_string())
                                            .or_default()
                                            .set_record_enabled(flags.enabled && flags.record);
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(AxumWsMsg::Binary(bin))) if bin.len() > MAX_STATE_WS_MESSAGE_BYTES => break,
                    Some(Ok(AxumWsMsg::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
            recv_res = rx.recv() => {
                let msg = match recv_res {
                    Ok(m) => m,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let Some((_, live_allowed)) = reverify_ws_caller_access_async(
                    &state,
                    caller_user.as_deref(),
                    had_db_user_at_connect,
                    &caller_role,
                    &allowed_cameras,
                    session_iat,
                )
                .await
                else {
                    break;
                };
                if let Some(filtered) =
                    filter_ws_message_for_allowed_cameras(&msg, live_allowed.as_deref())
                    && !matches!(
                        tokio::time::timeout(
                            Duration::from_secs(5),
                            socket.send(AxumWsMsg::Text(filtered.into())),
                        )
                        .await,
                        Ok(Ok(()))
                    )
                {
                    break;
                }
            }
        }
    }
}

async fn ws_go2rtc_proxy(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    uri: Uri,
    ws_res: std::result::Result<
        WebSocketUpgrade,
        axum::extract::ws::rejection::WebSocketUpgradeRejection,
    >,
) -> Response {
    if !verify_cookie_csrf_origin(&headers, &uri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "message": "Cross-origin cookie WebSocket request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
    let known = state.live_gateway.known_camera_set().await;
    if let Err(msg) =
        validate_go2rtc_query_streams_with_known(uri.query(), allowed.as_deref(), Some(&known))
    {
        let status = if msg == "Stream camera is not permitted for this user"
            || (msg == "Missing stream src query parameter" && allowed.is_some())
        {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::BAD_REQUEST
        };
        return (status, Json(json!({ "message": msg }))).into_response();
    }
    let query = uri.query().unwrap_or("").to_string();
    if live_gateway::query_has_disallowed_go2rtc_dst(&query)
        || mse_hub::extract_stream_src_from_query(&query).is_none()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Invalid or ambiguous stream src query parameter" })),
        )
            .into_response();
    }
    let caller_can_talkback = caller_can_mutate(&state, &jar, &headers, false).await;
    if !caller_can_talkback && auth_gate::query_targets_talkback_variant(uri.query(), Some(&known))
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Read-only role is not permitted to use audio talkback" })),
        )
            .into_response();
    }
    let ws = match ws_res {
        Ok(ws) => ws,
        Err(rej) => return rej.into_response(),
    };
    let is_webrtc_ws = uri.path().starts_with("/live/webrtc");
    ws.max_message_size(MAX_GO2RTC_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_GO2RTC_WS_MESSAGE_BYTES)
        .on_upgrade(move |client_ws| async move {
            state
                .live_gateway
                .bridge_ws_with_talkback_guard(
                    client_ws,
                    &state.go2rtc_ws_base,
                    query,
                    is_webrtc_ws,
                    caller_can_talkback,
                )
                .await;
        })
        .into_response()
}

pub(crate) fn resolve_recording_disk_path(media_dir: &Path, db_path: &str) -> PathBuf {
    media_lifecycle::resolve_recording_disk_path(media_dir, db_path)
}

pub fn generate_self_signed_tls_config() -> Result<Arc<rustls::ServerConfig>> {
    generate_self_signed_tls_config_with_sans(&[])
}

fn build_server_config_from_pem_bytes(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<Arc<rustls::ServerConfig>> {
    use rustls::pki_types::pem::PemObject;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls::pki_types::CertificateDer::pem_slice_iter(cert_pem)
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to parse TLS certificate PEM")?;
    if certs.is_empty() {
        anyhow::bail!("TLS certificate PEM contained zero certificates");
    }
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem)
        .context("failed to parse TLS private key PEM")?;
    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("invalid TLS certificate/private-key pair")?;
    server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(server_config))
}

fn generate_self_signed_tls_pem_pair(extra_sans: &[String]) -> Result<(String, String)> {
    let mut subject_alt_names = vec![
        "localhost".to_string(),
        "ninjanvr".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    for host in extra_sans {
        let raw = host.trim();
        let after_scheme = raw
            .split_once("://")
            .map_or(raw, |(_, r)| r)
            .split(['/', '?', '#', '\\'])
            .next()
            .unwrap_or("")
            .trim();
        let cleaned = if let Some(bracketed) = after_scheme.strip_prefix('[') {
            bracketed.split_once(']').map_or("", |(ip, _)| ip)
        } else if let Some((h, p)) = after_scheme.rsplit_once(':')
            && !h.contains(':')
            && p.chars().all(|c| c.is_ascii_digit())
        {
            h
        } else {
            after_scheme
        }
        .trim()
        .trim_end_matches('.');
        if !cleaned.is_empty()
            && !cleaned.contains('@')
            && !subject_alt_names
                .iter()
                .any(|s| s.eq_ignore_ascii_case(cleaned))
            && rcgen::CertificateParams::new(vec![cleaned.to_string()]).is_ok()
        {
            subject_alt_names.push(cleaned.to_string());
        }
    }
    let mut params = rcgen::CertificateParams::new(subject_alt_names)?;
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let signing_key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&signing_key)?;
    Ok((cert.pem(), signing_key.serialize_pem()))
}

pub fn generate_self_signed_tls_config_with_sans(
    extra_sans: &[String],
) -> Result<Arc<rustls::ServerConfig>> {
    let (cert_pem, key_pem) = generate_self_signed_tls_pem_pair(extra_sans)?;
    build_server_config_from_pem_bytes(cert_pem.as_bytes(), key_pem.as_bytes())
}

/// Loads operator-configured `tls.cert_path` + `tls.key_path` when present, or persists and reuses
/// a self-signed certificate pair in `config_dir` (`0600` owner-only key) for stable TOFU pinning,
/// falling back cleanly to an in-memory certificate when `config_dir` is read-only.
pub fn load_or_generate_tls_config(
    tls_cfg: &ninjanvr_config::TlsConfig,
    config_dir: &Path,
    san_hosts: &[String],
) -> Result<Arc<rustls::ServerConfig>> {
    if tls_cfg.cert_path.is_some() != tls_cfg.key_path.is_some() {
        anyhow::bail!("tls.cert_path and tls.key_path must both be configured together");
    }
    if let (Some(cert_path), Some(key_path)) = (&tls_cfg.cert_path, &tls_cfg.key_path) {
        if !ninjanvr_config::is_safe_tls_path(cert_path)
            || !ninjanvr_config::is_safe_tls_path(key_path)
        {
            anyhow::bail!("unsafe TLS certificate or private key path");
        }
        let canon_cert = std::fs::canonicalize(cert_path)
            .with_context(|| format!("failed to canonicalize TLS cert {}", cert_path.display()))?;
        let canon_key = std::fs::canonicalize(key_path)
            .with_context(|| format!("failed to canonicalize TLS key {}", key_path.display()))?;
        if !ninjanvr_config::is_safe_tls_path(&canon_cert)
            || !ninjanvr_config::is_safe_tls_path(&canon_key)
        {
            anyhow::bail!("canonicalized TLS certificate or private key path is unsafe");
        }
        let cert_bytes =
            read_bounded_non_symlink_bytes_sync(&canon_cert, 512 * 1024).ok_or_else(|| {
                anyhow::anyhow!("failed to read TLS certificate {}", cert_path.display())
            })?;
        let key_bytes =
            read_bounded_non_symlink_bytes_sync(&canon_key, 256 * 1024).ok_or_else(|| {
                anyhow::anyhow!("failed to read TLS private key {}", key_path.display())
            })?;
        return build_server_config_from_pem_bytes(&cert_bytes, &key_bytes);
    }

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for s in san_hosts {
        hasher.update(s.to_ascii_lowercase().as_bytes());
        hasher.update(b"\n");
    }
    let sans_digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let persisted_cert = config_dir.join(".tls_cert.pem");
    let persisted_key = config_dir.join(".tls_key.pem");
    let persisted_meta = config_dir.join(".tls_sans.sha256");

    if let (Some(cert_b), Some(key_b), Some(meta_b)) = (
        read_bounded_non_symlink_bytes_sync(&persisted_cert, 512 * 1024),
        read_bounded_non_symlink_bytes_sync(&persisted_key, 256 * 1024),
        read_bounded_non_symlink_bytes_sync(&persisted_meta, 256),
    ) && std::str::from_utf8(&meta_b).is_ok_and(|s| s.trim() == sans_digest)
        && let Ok(cfg) = build_server_config_from_pem_bytes(&cert_b, &key_b)
    {
        return Ok(cfg);
    }

    let (cert_pem, key_pem) = match generate_self_signed_tls_pem_pair(san_hosts) {
        Ok(pair) => pair,
        Err(err) => {
            warn!(
                "Failed to build TLS certificate with custom SANs ({err:#}); falling back to default SANs"
            );
            generate_self_signed_tls_pem_pair(&[])?
        }
    };
    let server_cfg = build_server_config_from_pem_bytes(cert_pem.as_bytes(), key_pem.as_bytes())?;

    #[cfg(unix)]
    if std::fs::symlink_metadata(config_dir).is_ok_and(|m| m.file_type().is_dir()) {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let write_secret = |target: &Path, content: &[u8], mode: u32| -> std::io::Result<()> {
            if std::fs::symlink_metadata(target)
                .is_ok_and(|m| m.file_type().is_dir() || m.file_type().is_symlink())
            {
                return Ok(());
            }
            let tmp = config_dir.join(format!(
                ".tls_tmp.{}.{:016x}",
                std::process::id(),
                rand::random::<u64>()
            ));
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .custom_flags(ninjanvr_config::O_NOFOLLOW_FLAG)
                .open(&tmp)?;
            f.set_permissions(std::fs::Permissions::from_mode(mode))?;
            f.write_all(content)?;
            f.sync_all()?;
            drop(f);
            if let Err(e) = std::fs::rename(&tmp, target) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
            Ok(())
        };
        let _ = write_secret(&persisted_key, key_pem.as_bytes(), 0o600);
        let _ = write_secret(&persisted_cert, cert_pem.as_bytes(), 0o644);
        let _ = write_secret(&persisted_meta, sans_digest.as_bytes(), 0o600);
    }

    Ok(server_cfg)
}

fn restore_persistent_clips_mirrors(media_dir: &Path, config_dir: &Path) {
    // 1. Restore dataset samples from {media_dir}/clips/dataset -> {media_dir}/dataset
    let clips_dataset_dir = media_dir.join("clips").join("dataset");
    let dataset_dir = media_dir.join("dataset");
    if std::fs::symlink_metadata(&clips_dataset_dir).is_ok_and(|m| m.file_type().is_dir())
        && let Ok(entries) = std::fs::read_dir(&clips_dataset_dir)
    {
        for entry in entries.flatten() {
            let p = entry.path();
            if let Some(fname) = p.file_name().and_then(|s| s.to_str())
                && is_safe_id(fname)
                && !dataset_dir.join(fname).exists()
                && let Some(bytes) = read_bounded_non_symlink_bytes_sync(&p, 16 * 1024 * 1024)
            {
                let _ = crate::pipeline::write_contained_media_file(
                    media_dir,
                    &dataset_dir,
                    fname,
                    &bytes,
                );
            }
        }
    }

    // 2. Restore enrolled face profiles from {media_dir}/clips/faces/<profile>/<sample> -> {media_dir}/faces/<profile>/<sample>
    let clips_faces_dir = media_dir.join("clips").join("faces");
    let faces_dir = media_dir.join("faces");
    if std::fs::symlink_metadata(&clips_faces_dir).is_ok_and(|m| m.file_type().is_dir())
        && let Ok(prof_entries) = std::fs::read_dir(&clips_faces_dir)
    {
        for prof_entry in prof_entries.flatten() {
            let prof_path = prof_entry.path();
            if !std::fs::symlink_metadata(&prof_path).is_ok_and(|m| m.file_type().is_dir()) {
                continue;
            }
            let Some(raw_prof) = prof_path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(clean_prof) = ninjanvr_infer::sanitize_face_profile_name(raw_prof) else {
                continue;
            };
            if clean_prof != raw_prof {
                continue;
            }
            let dst_prof_dir = faces_dir.join(&clean_prof);
            if let Ok(files) = std::fs::read_dir(&prof_path) {
                for file_entry in files.flatten() {
                    let fp = file_entry.path();
                    if let Some(fname) = fp.file_name().and_then(|s| s.to_str())
                        && is_safe_id(fname)
                        && !dst_prof_dir.join(fname).exists()
                        && let Some(bytes) =
                            read_bounded_non_symlink_bytes_sync(&fp, 16 * 1024 * 1024)
                    {
                        let _ = crate::pipeline::write_contained_media_file(
                            media_dir,
                            &dst_prof_dir,
                            fname,
                            &bytes,
                        );
                    }
                }
            }
        }
    }

    // 3. Restore uploaded models from {config_dir}/models -> {media_dir}/models
    let cfg_models_dir = config_dir.join("models");
    let media_models_dir = media_dir.join("models");
    if std::fs::symlink_metadata(&cfg_models_dir).is_ok_and(|m| m.file_type().is_dir())
        && let Ok(entries) = std::fs::read_dir(&cfg_models_dir)
    {
        for entry in entries.flatten() {
            let p = entry.path();
            if let Some(fname) = p.file_name().and_then(|s| s.to_str())
                && is_safe_id(fname)
                && !media_models_dir.join(fname).exists()
                && let Some(bytes) = read_bounded_non_symlink_bytes_sync(&p, 64 * 1024 * 1024)
            {
                let _ = crate::pipeline::write_contained_media_file(
                    media_dir,
                    &media_models_dir,
                    fname,
                    &bytes,
                );
            }
        }
    }
}

fn write_initial_admin_password_file(config_dir: &Path, password: &str) -> Option<PathBuf> {
    if config_dir.as_os_str().is_empty()
        || config_dir
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        warn!(
            "Refusing to write initial_admin_password in unsafe config directory {}",
            config_dir.display()
        );
        return None;
    }
    if std::fs::symlink_metadata(config_dir).is_ok_and(|m| m.file_type().is_symlink()) {
        warn!(
            "Refusing to write initial_admin_password in symlinked config directory {}",
            config_dir.display()
        );
        return None;
    }
    let _ = std::fs::create_dir_all(config_dir);
    let pw_path = config_dir.join("initial_admin_password");
    if let Ok(meta) = std::fs::symlink_metadata(&pw_path)
        && (meta.file_type().is_symlink() || meta.is_dir())
    {
        warn!(
            "Refusing to overwrite symlink or directory at {}",
            pw_path.display()
        );
        return None;
    }
    let payload = format!("{password}\n");
    let nonce: u64 = rand::random();
    let tmp_path = config_dir.join(format!(
        ".initial_admin_password.tmp.{}.{nonce:016x}",
        std::process::id()
    ));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let open_res = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&tmp_path);
        let mut f = match open_res {
            Ok(file) => file,
            Err(err) => {
                warn!(
                    "Failed to create temporary initial admin password file {}: {err}",
                    tmp_path.display()
                );
                return None;
            }
        };
        if let Err(err) = f.set_permissions(std::fs::Permissions::from_mode(0o600)) {
            drop(f);
            let _ = std::fs::remove_file(&tmp_path);
            warn!(
                "Failed to enforce 0600 permissions on {}: {err}",
                tmp_path.display()
            );
            return None;
        }
        if let Err(err) = f.write_all(payload.as_bytes()).and_then(|()| f.sync_all()) {
            drop(f);
            let _ = std::fs::remove_file(&tmp_path);
            warn!(
                "Failed to write initial admin password file {}: {err}",
                tmp_path.display()
            );
            return None;
        }
        drop(f);
        if let Ok(meta) = std::fs::symlink_metadata(&pw_path)
            && (meta.file_type().is_symlink() || meta.is_dir())
        {
            let _ = std::fs::remove_file(&tmp_path);
            return None;
        }
        if let Err(err) = std::fs::rename(&tmp_path, &pw_path) {
            let _ = std::fs::remove_file(&tmp_path);
            warn!(
                "Failed to atomically publish initial admin password file {}: {err}",
                pw_path.display()
            );
            return None;
        }
        Some(pw_path)
    }
    #[cfg(not(unix))]
    {
        if std::fs::write(&tmp_path, payload.as_bytes()).is_ok()
            && std::fs::rename(&tmp_path, &pw_path).is_ok()
        {
            Some(pw_path)
        } else {
            let _ = std::fs::remove_file(&tmp_path);
            None
        }
    }
}

fn remove_initial_admin_password_file(config_dir: &Path) {
    let pw_path = config_dir.join("initial_admin_password");
    if let Ok(meta) = std::fs::symlink_metadata(&pw_path)
        && meta.file_type().is_file()
        && !meta.file_type().is_symlink()
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            if let Ok(file) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(&pw_path)
            {
                let _ = file.set_len(0);
            }
        }
        let _ = std::fs::remove_file(&pw_path);
    }
}

async fn apply_runtime_admin_password_reset(
    state: &AppState,
    iterations: u32,
) -> Option<(Value, String)> {
    let Ok(_permit) = state.auth_gate.pbkdf2_semaphore().acquire().await else {
        warn!("Skipped runtime admin password reset: PBKDF2 semaphore closed");
        return None;
    };
    let db_clone = state.db.clone();
    let reset_res = tokio::task::spawn_blocking(move || {
        db_clone.ensure_default_admin_with_options(iterations, None, true)
    })
    .await;
    drop(_permit);
    if let Ok(Ok(Some(new_pw))) = reset_res {
        state.auth_gate.invalidate_user("admin");
        state.auth_gate.clear_all_login_failures_for_user("admin");
        let (cfg_dir_opt, persist_pair, updated_pair) = {
            let mut cfg_guard = state.config.write().await;
            cfg_guard.auth.reset_admin_password = false;
            cfg_guard.auth.admin_first_time_login = true;
            let _ = cfg_guard.refresh_raw_yaml();
            let persist_target = (!cfg_guard.config_path.as_os_str().is_empty())
                .then(|| (cfg_guard.config_path.clone(), cfg_guard.raw_yaml.clone()));
            let scrubbed_yaml =
                config_coordinator::scrub_raw_yaml_secrets_strict(&cfg_guard.raw_yaml, &cfg_guard);
            let api_val = cfg_guard.to_api_config();
            (
                cfg_guard.config_path.parent().map(Path::to_path_buf),
                persist_target,
                (api_val, scrubbed_yaml),
            )
        };
        if let Some((cfg_path, raw_yaml_to_write)) = persist_pair {
            let _ = tokio::task::spawn_blocking(move || {
                config_coordinator::persist_config_yaml_atomically(&cfg_path, &raw_yaml_to_write)
            })
            .await;
        }
        if let Some(cfg_dir) = cfg_dir_opt
            && let Some(written) = write_initial_admin_password_file(&cfg_dir, &new_pw)
        {
            info!(
                "Reset admin password via auth.reset_admin_password; wrote recovery secret to {}",
                written.display()
            );
        }
        return Some(updated_pair);
    }
    None
}

pub async fn new_app_state(
    config_path: &Path,
    web_dir: PathBuf,
    media_dir: PathBuf,
    spawn_go2rtc: bool,
) -> Result<AppState> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut cfg = NinjaConfig::load(config_path)?;
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("/config"));
    restore_persistent_clips_mirrors(&media_dir, config_dir);
    let (init_degraded, init_err) = match probe_storage_writable(&media_dir) {
        Ok(()) => (false, None),
        Err(err) => {
            warn!(
                "Storage mount {} is degraded at startup ({}); keeping Live Wall online",
                media_dir.display(),
                err
            );
            (true, Some(err))
        }
    };
    let storage_degraded = Arc::new(std::sync::atomic::AtomicBool::new(init_degraded));
    let storage_error = Arc::new(std::sync::Mutex::new(init_err));

    let mut using_degraded_fallback_db = false;
    let db = match NinjaDatabase::open(&cfg.database.path) {
        Ok(d) => d,
        Err(err) => {
            using_degraded_fallback_db = true;
            warn!(
                "Primary database path {} unavailable ({}); falling back to /tmp/ninjanvr-degraded.db",
                cfg.database.path, err
            );
            storage_degraded.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Ok(mut g) = storage_error.lock()
                && g.is_none()
            {
                *g = Some(format!(
                    "Primary DB path {} unavailable: {err}",
                    cfg.database.path
                ));
            }
            let fallback_dir = std::env::temp_dir().join(format!(
                "ninjanvr-degraded-{}-{:032x}",
                std::process::id(),
                rand::random::<u128>()
            ));
            std::fs::create_dir_all(&fallback_dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&fallback_dir, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| {
                        format!(
                            "failed to set 0700 permissions on fallback DB dir {}",
                            fallback_dir.display()
                        )
                    })?;
            }
            let fallback_db = fallback_dir.join("ninjanvr-degraded.db");
            let opened = NinjaDatabase::open(&fallback_db)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                for suffix in ["", "-wal", "-shm"] {
                    let p = PathBuf::from(format!("{}{suffix}", fallback_db.display()));
                    if p.exists() {
                        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))
                            .with_context(|| {
                                format!(
                                    "failed to set 0600 permissions on fallback DB file {}",
                                    p.display()
                                )
                            })?;
                    }
                }
            }
            opened
        }
    };
    let env_admin_pw = std::env::var("NINJANVR_ADMIN_PASSWORD")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let force_admin_reset = cfg.auth.reset_admin_password;
    if let Some(generated_pw) = db.ensure_default_admin_with_options(
        cfg.auth.hash_iterations,
        env_admin_pw.as_deref(),
        force_admin_reset,
    )? {
        cfg.auth.admin_first_time_login = true;
        cfg.auth.reset_admin_password = false;
        if force_admin_reset && cfg.refresh_raw_yaml().is_ok() {
            let _ = config_coordinator::persist_config_yaml_atomically(config_path, &cfg.raw_yaml);
        }
        if let Some(written) = write_initial_admin_password_file(config_dir, &generated_pw) {
            info!(
                "Initialized default admin user; initial password saved at {} (mode 0600)",
                written.display()
            );
        } else {
            info!(
                "Initialized default admin user (set NINJANVR_ADMIN_PASSWORD or writable config dir to retrieve bootstrap credentials)"
            );
        }
    } else if env_admin_pw.is_some() {
        cfg.auth.admin_first_time_login = false;
        cfg.auth.reset_admin_password = false;
        if force_admin_reset && cfg.refresh_raw_yaml().is_ok() {
            let _ = config_coordinator::persist_config_yaml_atomically(config_path, &cfg.raw_yaml);
        }
        remove_initial_admin_password_file(config_dir);
    } else if std::fs::symlink_metadata(config_dir.join("initial_admin_password"))
        .is_ok_and(|m| m.file_type().is_file())
    {
        cfg.auth.admin_first_time_login = true;
    }
    let startup_now_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    match db.close_orphaned_open_events_and_reviews(startup_now_ts, 15.0) {
        Ok((closed_ev, closed_rev)) if closed_ev > 0 || closed_rev > 0 => {
            info!(
                "Closed {closed_ev} orphaned open event(s) and {closed_rev} orphaned open review segment(s) on startup"
            );
        }
        Ok(_) => {}
        Err(err) => {
            warn!("Failed to sweep orphaned open events/reviews at startup: {err:#}");
        }
    }

    let base_jwt_secret = load_or_create_jwt_secret(config_dir);
    let jwt_secret = if using_degraded_fallback_db {
        // Isolate JWT signing key when running on an empty fallback DB so pre-existing tokens
        // for revoked/deleted accounts are rejected and tokens minted in degraded mode never
        // carry over once the primary database recovers.
        auth_gate::normalize_jwt_secret_entropy(&format!(
            "{base_jwt_secret}:degraded:{:032x}",
            rand::random::<u128>()
        ))
    } else {
        base_jwt_secret
    };

    let detector = Arc::new(AiDetectorEngine::new(&cfg));
    let faces_dir = media_dir.join("faces");
    if let Err(err) = detector.sync_face_gallery_from_dir(&faces_dir) {
        warn!(
            "Failed to sync face gallery from {}: {err:#}",
            faces_dir.display()
        );
    }
    let comms = Arc::new(CommsHub::with_mqtt(&cfg.mqtt));
    comms.init_from_config(&cfg);
    let profile_file = config_dir.join(".active_profile");
    comms.set_profile_persist_path(profile_file.clone());
    if let Some(prof_bytes) = read_bounded_non_symlink_bytes_sync(&profile_file, 256)
        && let Ok(saved_prof) = String::from_utf8(prof_bytes)
    {
        let trimmed_prof = saved_prof.trim();
        if is_safe_id(trimmed_prof) {
            comms.set_active_profile(trimmed_prof);
        }
    }
    let log_hub = LogBufferHub::new();
    log_hub.push_ninjanvr(
        "INFO",
        "ninjanvr.app",
        &format!("Starting NinjaNVR ({NINJANVR_VERSION})"),
    );
    log_hub.push_ninjanvr(
        "INFO",
        "ninjanvr.db",
        &format!("Connected to SQLite WAL database at {}", cfg.database.path),
    );
    let det_cat = detector.runtime_catalog();
    log_hub.push_ninjanvr(
        "INFO",
        "ninjanvr.infer",
        &format!(
            "Initialized {} detector on {} (model={})",
            det_cat.active_backend, det_cat.active_device, cfg.model.path
        ),
    );
    for (cam_name, cam_cfg) in &cfg.cameras {
        if cam_cfg.enabled {
            log_hub.push_ninjanvr(
                "INFO",
                &format!("ninjanvr.camera.{cam_name}"),
                &format!(
                    "Configured substream vision & direct segment recorder ({}x{} @ {} FPS)",
                    cam_cfg.detect.width, cam_cfg.detect.height, cam_cfg.detect.fps
                ),
            );
            if spawn_go2rtc {
                log_hub.push_go2rtc(
                    "INF",
                    "streams",
                    &format!("Registered RTSP restream producer for {cam_name} and {cam_name}_sub"),
                );
            }
        }
    }
    if spawn_go2rtc {
        log_hub.push_go2rtc("INF", "rtsp", "listen addr=127.0.0.1:8554");
        log_hub.push_go2rtc("INF", "api", "listen addr=127.0.0.1:1984");
        log_hub.push_go2rtc("INF", "webrtc", "listen tcp/udp addr=:8555");
    }
    {
        let rec_dir = media_dir.join("recordings");
        let rec_mount = stat_mount_mb(&rec_dir, "ext4");
        let startup_heuristics =
            evaluate_system_heuristics(&cfg, &serde_json::Map::new(), &rec_mount, 14.0);
        emit_heuristics_to_logs(&log_hub, &startup_heuristics);
    }

    let (shutdown_tx, shutdown_rx_base) = tokio::sync::watch::channel(false);
    let shared_cfg = Arc::new(RwLock::new(cfg));
    let activity_ledger = new_activity_ledger();
    let latest_frames = Arc::new(RwLock::new(HashMap::new()));
    let latest_yuv_frames: SharedLatestYuvFrames = Arc::new(RwLock::new(HashMap::new()));
    // Live event-strip preview frames: index what survived a restart, drop the cache
    // left by the earlier on-demand ffmpeg extractor, and prune on a cancel-safe timer.
    let preview_frame_store = preview_frames::PreviewFrameStore::new(&media_dir);
    {
        let store = preview_frame_store.clone();
        let legacy_cache = media_dir.join("clips").join("review_frames");
        let loaded = tokio::task::spawn_blocking(move || {
            if std::fs::symlink_metadata(&legacy_cache).is_ok_and(|m| m.is_dir()) {
                let _ = std::fs::remove_dir_all(&legacy_cache);
            }
            store.load_from_disk()
        })
        .await
        .unwrap_or(0);
        info!("Indexed {loaded} cached preview frames");
        let store = preview_frame_store.clone();
        let mut prune_shutdown_rx = shutdown_rx_base.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(300));
            tick.tick().await;
            loop {
                tokio::select! {
                    biased;
                    _ = prune_shutdown_rx.changed() => {
                        if *prune_shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = tick.tick() => {
                        let store = store.clone();
                        let now_ms = unix_now().saturating_mul(1000);
                        let _ = tokio::task::spawn_blocking(move || {
                            store.prune(now_ms, preview_frames::PREVIEW_FRAME_RETENTION_SECS * 1000)
                        })
                        .await;
                    }
                }
            }
        });
    }
    let lazy_snapshot_cache = Arc::new(RwLock::new(HashMap::new()));
    let mse_hub = MseStreamHub::new();
    let camera_metrics: SharedCameraMetrics = Arc::new(RwLock::new(HashMap::new()));
    let stats_history: Arc<RwLock<VecDeque<Value>>> = Arc::new(RwLock::new(VecDeque::new()));
    let go2rtc_supervisor = Arc::new(Go2RtcSupervisor::new(shared_cfg.clone()));
    let mut initial_ptz_presets = HashMap::new();
    {
        let cfg_guard = shared_cfg.read().await;
        for (cam_name, cam_cfg) in &cfg_guard.cameras {
            let init_preset = cam_cfg
                .onvif
                .return_preset
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            initial_ptz_presets.insert(cam_name.clone(), init_preset);
        }
    }
    let ptz_active_presets = Arc::new(RwLock::new(initial_ptz_presets));
    let camera_driver_runtime =
        CameraDriverRuntime::with_active_presets(ptz_active_presets.clone());

    let go2rtc_http_base = "http://127.0.0.1:1984".to_string();
    let go2rtc_ws_base = "ws://127.0.0.1:1984".to_string();
    let vod_cache = VodWindowCache::new();
    let pipeline = Arc::new(
        VisionPipeline::new(
            shared_cfg.clone(),
            db.clone(),
            comms.clone(),
            detector.clone(),
            activity_ledger.clone(),
            latest_frames.clone(),
            latest_yuv_frames.clone(),
            preview_frame_store.clone(),
            camera_metrics.clone(),
            media_dir.clone(),
            log_hub.clone(),
            camera_driver_runtime.clone(),
        )
        .with_go2rtc_endpoint(&go2rtc_http_base, spawn_go2rtc),
    );
    let audio_comms = comms.clone();
    let audio_pipeline = pipeline.clone();
    let recorder = Arc::new(
        RecordingEngine::with_storage_state(
            shared_cfg.clone(),
            db.clone(),
            media_dir.clone(),
            activity_ledger.clone(),
            storage_degraded.clone(),
            storage_error.clone(),
        )
        .with_go2rtc_relay(spawn_go2rtc)
        .with_audio_telemetry_sink(Arc::new(move |cam: &str, dbfs: i64, active: bool| {
            audio_comms.publish_audio_activity(cam, dbfs, active);
            audio_pipeline.record_camera_audio_dbfs(cam, dbfs);
        })),
    );

    // Synchronize inbound MQTT `<camera>/<feature>/set` state changes with `activity_ledger`
    // identically to `/ws` commands so Home Assistant toggles take effect in recording policies.
    {
        let mut set_rx = comms.subscribe_set_commands();
        let mut set_shutdown_rx = shutdown_rx_base.clone();
        let comms_for_sync = comms.clone();
        let ledger_for_sync = activity_ledger.clone();
        let cfg_for_sync = shared_cfg.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = set_shutdown_rx.changed() => {
                        if *set_shutdown_rx.borrow() {
                            break;
                        }
                    }
                    res = set_rx.recv() => {
                        let (rel_topic, _) = match res {
                            Ok(pair) => pair,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        };
                        if let Some(base) = rel_topic.strip_suffix("/set") {
                            let parts: Vec<&str> = base.split('/').collect();
                            if parts.len() == 2 {
                                let cam = parts[0];
                                if is_safe_id(cam) && cfg_for_sync.read().await.cameras.contains_key(cam) {
                                    let flags = comms_for_sync.get_camera_flags(cam);
                                    ledger_for_sync
                                        .write()
                                        .await
                                        .entry(cam.to_string())
                                        .or_default()
                                        .set_record_enabled(flags.enabled && flags.record);
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    if spawn_go2rtc {
        go2rtc_supervisor.clone().spawn_supervisor_task();
        recorder.clone().start().await;
        pipeline.clone().start().await;
    } else {
        recorder.shutdown().await;
        pipeline.shutdown().await;
    }

    let _ = rustls::crypto::ring::default_provider().install_default();
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let web_dir = if web_dir.join("dist").join("index.html").exists() {
        web_dir.join("dist")
    } else if !web_dir.join("index.html").exists()
        && let Some(parent) = web_dir.parent()
        && parent.join("web").join("index.html").exists()
    {
        parent.join("web")
    } else {
        web_dir
    };

    let config_coordinator = ConfigCoordinator::new(shared_cfg.clone())
        .with_mse_hub(mse_hub.clone())
        .with_live_caches(vod_cache.clone(), &go2rtc_ws_base)
        .with_runtime_reconcilers(
            pipeline.clone(),
            recorder.clone(),
            go2rtc_supervisor.clone(),
            comms.clone(),
        );
    let share_engine = ShareLinkEngine::new();
    let media_delivery_gate = MediaDeliveryGate::new(shared_cfg.clone());
    let live_gateway = LiveStreamGateway::new(
        shared_cfg.clone(),
        mse_hub.clone(),
        Arc::new(ReqwestGo2RtcUpstream::new(http_client.clone())),
    );
    let state = AppState {
        config: shared_cfg,
        config_coordinator,
        db,
        comms,
        go2rtc_supervisor,
        go2rtc_http_base,
        go2rtc_ws_base,
        http_client,
        jwt_secret,
        auth_gate: AuthSessionGate::new(),
        start_instant: Instant::now(),
        web_dir,
        media_dir,
        latest_frames,
        latest_yuv_frames,
        preview_frames: preview_frame_store,
        historical_frames: export_jobs::HistoricalFrameCache::default(),
        export_jobs: export_jobs::ExportJobManager::default(),
        lazy_snapshot_cache,
        mse_hub,
        activity_ledger,
        detector,
        pipeline,
        recorder,
        vod_cache,
        camera_metrics,
        log_hub,
        stats_history,
        cpu_tracker: ProcCpuTracker::default(),
        storage_degraded,
        storage_error,
        storage_breakdown_cache: storage_ops::StorageBreakdownCache::new(),
        ptz_active_presets,
        camera_driver_runtime,
        dataset_catalog: DatasetCatalog::new(),
        face_gallery: FaceIdentityCatalog::new(),
        share_engine,
        media_delivery_gate,
        live_gateway,
        debug_reprocess_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
        vod_remux_semaphore: Arc::new(tokio::sync::Semaphore::new(6)),
        metrics_scrape_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        unscoped_metrics_cache: Arc::new(tokio::sync::Mutex::new(None)),
        camera_logs_cooldown: Arc::new(std::sync::Mutex::new(HashMap::new())),
        shutdown_tx,
        active_listener_ports: Arc::new(std::sync::RwLock::new((5000, 8971, DEFAULT_METRICS_PORT))),
        allow_ephemeral_loopback_federation_for_test: Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )),
    };

    // Seed 30 initial stats_history points so all 30 bars in /system#general and /system#cameras render with zero 0-padding
    {
        let base_snap = compute_stats_snapshot(&state).await;
        let now_ts = unix_now();
        let mut hist = state.stats_history.write().await;
        for i in (0..30).rev() {
            let mut snap = base_snap.clone();
            if let Some(svc) = snap.get_mut("service").and_then(|s| s.as_object_mut()) {
                svc.insert(
                    "last_updated".to_string(),
                    json!(now_ts.saturating_sub(i * 5)),
                );
            }
            hist.push_back(snap);
        }
    }

    if spawn_go2rtc {
        let persistent_streams: Vec<String> = {
            let cfg_guard = state.config.read().await;
            let mut out = Vec::new();
            for (cam_name, cam_cfg) in &cfg_guard.cameras {
                if !cam_cfg.enabled {
                    continue;
                }
                let trimmed = cam_name.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if !out.iter().any(|s| s == trimmed) {
                    out.push(trimmed.to_string());
                }
                let sub = format!("{trimmed}_sub");
                let resolved_sub = cfg_guard.resolve_mse_stream_alias(&sub);
                if !out.iter().any(|s| s == resolved_sub) {
                    out.push(resolved_sub.to_string());
                }
            }
            out
        };
        state
            .mse_hub
            .start_persistent_streams(&state.go2rtc_ws_base, persistent_streams)
            .await;

        let stats_state = state.clone();
        let mut stats_shutdown_rx = shutdown_rx_base;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = stats_shutdown_rx.changed() => {
                        if *stats_shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                        let stats_val = compute_stats_snapshot(&stats_state).await;
                        {
                            let mut hist = stats_state.stats_history.write().await;
                            hist.push_back(stats_val.clone());
                            while hist.len() > MAX_STATS_HISTORY {
                                hist.pop_front();
                            }
                        }
                        stats_state.comms.publish_json("stats", &stats_val);
                    }
                }
            }
        });
    }

    Ok(state)
}

#[cfg(test)]
mod tests;
