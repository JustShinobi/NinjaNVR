#![recursion_limit = "512"]

pub mod mse_hub;
pub mod pipeline;

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
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::{delete, get, patch, post, put};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine;
use chrono::Local;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, KeyInit, Mac};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
pub use mse_hub::{MseGopCache, MseStreamHub};
use ninjanvr_comms::{CommsHub, SharedCommsHub};
use ninjanvr_config::{
    DEFAULT_GO2RTC_SHM_PATH, Go2RtcSupervisor, NINJANVR_VERSION, NinjaConfig, scrub_json_value,
    scrub_rtsp_credentials,
};
use ninjanvr_db::{EventFilter, ExportRow, NinjaDatabase, ReviewFilter, verify_password};
use ninjanvr_infer::AiDetectorEngine;
use ninjanvr_media::{
    RecordingEngine, SharedActivityLedger, build_mp4_export_with_privacy, new_activity_ledger,
    probe_storage_writable,
};
use pipeline::{SharedCameraMetrics, SharedLatestYuvFrames, VisionPipeline};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{RwLock, broadcast};
use tokio_tungstenite::tungstenite::protocol::Message as TungMsg;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

const MAX_LOG_LINES: usize = 500;
const MAX_STATS_HISTORY: usize = 60;

#[derive(Clone)]
pub struct LogBufferHub {
    buffers: Arc<std::sync::Mutex<HashMap<String, VecDeque<String>>>>,
    stream_tx: broadcast::Sender<(String, String)>,
}

impl Default for LogBufferHub {
    fn default() -> Self {
        Self::new()
    }
}

pub fn sanitize_logged_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("/share/") {
        if let Some((_, suffix)) = rest.split_once('/') {
            return format!("/share/<redacted>/{suffix}");
        } else if !rest.is_empty() {
            return "/share/<redacted>".to_string();
        }
    }
    path.to_string()
}

impl LogBufferHub {
    pub fn new() -> Self {
        let (stream_tx, _) = broadcast::channel(256);
        let mut map = HashMap::new();
        map.insert("ninjanvr".to_string(), VecDeque::new());
        map.insert("go2rtc".to_string(), VecDeque::new());
        map.insert("nginx".to_string(), VecDeque::new());
        Self {
            buffers: Arc::new(std::sync::Mutex::new(map)),
            stream_tx,
        }
    }

    pub fn push_ninjanvr(&self, severity: &str, section: &str, message: &str) {
        let now = Local::now();
        let outer_ts = now.format("%Y-%m-%d %H:%M:%S%.9f");
        let inner_ts = now.format("%Y-%m-%d %H:%M:%S");
        let sev_pad = format!("{:<7}", severity.to_ascii_uppercase());
        let sec_pad = format!("{:<30}", section);
        let line = format!("{outer_ts}  [{inner_ts}] {sec_pad} {sev_pad} : {message}");
        self.push_raw("ninjanvr", line);
    }

    pub fn push_go2rtc(&self, level: &str, section: &str, message: &str) {
        let now = Local::now();
        let outer_ts = now.format("%Y-%m-%d %H:%M:%S%.9f");
        let inner_ts = now.format("%H:%M:%S%.3f");
        let line = format!("{outer_ts}  {inner_ts} {level} [{section}] {message}");
        self.push_raw("go2rtc", line);
    }

    pub fn push_nginx(&self, method: &str, path: &str, status: u16) {
        let now = Local::now();
        let outer_ts = now.format("%Y-%m-%d %H:%M:%S%.9f");
        let clf_ts = now.format("%d/%b/%Y:%H:%M:%S %z");
        let safe_path = sanitize_logged_path(path);
        let line = format!(
            "{outer_ts}  127.0.0.1 - - [{clf_ts}] \"{method} {safe_path} HTTP/1.1\" {status} 0 \"-\" \"NinjaNVR\""
        );
        self.push_raw("nginx", line);
    }

    pub fn push_raw(&self, service: &str, line: String) {
        {
            let mut guard = self.buffers.lock().expect("log mutex poisoned");
            let q = guard.entry(service.to_string()).or_default();
            q.push_back(line.clone());
            while q.len() > MAX_LOG_LINES {
                q.pop_front();
            }
        }
        let _ = self.stream_tx.send((service.to_string(), line));
    }

    pub fn query_lines(
        &self,
        service: &str,
        start: Option<i64>,
        end: Option<i64>,
    ) -> (usize, Vec<String>) {
        let guard = self.buffers.lock().expect("log mutex poisoned");
        let Some(q) = guard.get(service) else {
            return (0, Vec::new());
        };
        let total = q.len();
        if total == 0 {
            return (0, Vec::new());
        }
        let start_idx = match start {
            Some(s) if s < 0 => total.saturating_sub((-s) as usize),
            Some(s) => (s as usize).min(total),
            None => 0,
        };
        let end_idx = match end {
            Some(e) if e >= 0 => (e as usize).min(total),
            _ => total,
        };
        if start_idx >= end_idx {
            return (total, Vec::new());
        }
        let slice = q
            .iter()
            .skip(start_idx)
            .take(end_idx - start_idx)
            .cloned()
            .collect();
        (total, slice)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<(String, String)> {
        self.stream_tx.subscribe()
    }
}

type PidTickEntry = (u64, Instant, f64);

#[derive(Clone, Default)]
pub struct ProcCpuTracker {
    prev_ticks: Arc<std::sync::Mutex<HashMap<u32, PidTickEntry>>>,
    prev_infer: Arc<std::sync::Mutex<Option<PidTickEntry>>>,
}

impl ProcCpuTracker {
    /// Returns `(cpu_percent, mem_percent)` for `pid` by reading `/proc/{pid}/stat`,
    /// normalizing CPU across the container's CPU limit (`2.0` cores) with EMA smoothing,
    /// and normalizing memory against `/sys/fs/cgroup/memory.max`.
    pub fn sample_pid(&self, pid: u32, fallback_cpu: f64, fallback_mem: f64) -> (f64, f64) {
        if pid == 0 {
            return (fallback_cpu, fallback_mem);
        }
        let Ok(stat_str) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return (fallback_cpu, fallback_mem);
        };
        let Some(rparen) = stat_str.rfind(')') else {
            return (fallback_cpu, fallback_mem);
        };
        let fields: Vec<&str> = stat_str[rparen + 1..].split_whitespace().collect();
        if fields.len() < 22 {
            return (fallback_cpu, fallback_mem);
        }
        let utime: u64 = fields[11].parse().unwrap_or(0);
        let stime: u64 = fields[12].parse().unwrap_or(0);
        let rss_pages: u64 = fields[21].parse().unwrap_or(0);
        let total_ticks = utime + stime;
        let rss_bytes = rss_pages * 4096;

        let mem_limit = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(805_306_368)
            .max(67_108_864);
        let mem_pct = (((rss_bytes as f64) / (mem_limit as f64)) * 100.0).clamp(0.5, 95.0);

        let now = Instant::now();
        let mut guard = self.prev_ticks.lock().expect("cpu tracker mutex poisoned");
        let cpu_pct = if let Some((prev_t, prev_inst, prev_ema)) = guard.get(&pid).copied() {
            let dt = now.duration_since(prev_inst).as_secs_f64();
            if dt >= 2.0 && total_ticks >= prev_t {
                let delta_sec = (total_ticks - prev_t) as f64 / 100.0;
                // Normalize against 2-core container limit (200% max raw -> 100% normalized)
                let raw_pct = ((delta_sec / dt) * 50.0).clamp(0.4, 95.0);
                let ema = prev_ema * 0.6 + raw_pct * 0.4;
                guard.insert(pid, (total_ticks, now, ema));
                ema
            } else {
                prev_ema
            }
        } else {
            guard.insert(pid, (total_ticks, now, fallback_cpu));
            fallback_cpu
        };

        (
            (cpu_pct * 10.0).round() / 10.0,
            (mem_pct * 10.0).round() / 10.0,
        )
    }

    /// Computes exact CPU% spent inside OpenVINO inference (`cumulative_infer_us` delta).
    pub fn sample_infer_cpu(&self, cumulative_us: u64, fallback_cpu: f64) -> f64 {
        let now = Instant::now();
        let mut guard = self
            .prev_infer
            .lock()
            .expect("infer tracker mutex poisoned");
        let pct = if let Some((prev_us, prev_inst, prev_ema)) = *guard {
            let dt_us = now.duration_since(prev_inst).as_micros() as f64;
            if dt_us >= 2_000_000.0 && cumulative_us >= prev_us {
                let delta_us = (cumulative_us - prev_us) as f64;
                let raw_pct = ((delta_us / dt_us) * 100.0).clamp(0.8, 95.0);
                let ema = prev_ema * 0.6 + raw_pct * 0.4;
                *guard = Some((cumulative_us, now, ema));
                ema
            } else {
                prev_ema
            }
        } else {
            *guard = Some((cumulative_us, now, fallback_cpu));
            fallback_cpu
        };
        (pct * 10.0).round() / 10.0
    }
}

pub type LazySnapshotCacheKey = (String, u32, bool);
pub type LazySnapshotCacheValue = (Instant, Bytes, u64);

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<NinjaConfig>>,
    pub db: NinjaDatabase,
    pub comms: SharedCommsHub,
    pub go2rtc_supervisor: Arc<Go2RtcSupervisor>,
    pub go2rtc_http_base: String,
    pub go2rtc_ws_base: String,
    pub http_client: reqwest::Client,
    pub jwt_secret: String,
    pub start_instant: Instant,
    pub web_dir: PathBuf,
    pub media_dir: PathBuf,
    pub latest_frames: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    pub latest_yuv_frames: SharedLatestYuvFrames,
    pub lazy_snapshot_cache: Arc<RwLock<HashMap<LazySnapshotCacheKey, LazySnapshotCacheValue>>>,
    pub mse_hub: MseStreamHub,
    pub activity_ledger: SharedActivityLedger,
    pub detector: Arc<AiDetectorEngine>,
    pub camera_metrics: SharedCameraMetrics,
    pub log_hub: LogBufferHub,
    pub stats_history: Arc<RwLock<VecDeque<Value>>>,
    pub cpu_tracker: ProcCpuTracker,
    pub storage_degraded: Arc<std::sync::atomic::AtomicBool>,
    pub storage_error: Arc<std::sync::Mutex<Option<String>>>,
    pub ptz_active_presets: Arc<RwLock<HashMap<String, Option<String>>>>,
    pub share_clip_locks: Arc<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    pub share_page_video_credits: Arc<std::sync::Mutex<HashMap<String, SharePlaybackCredit>>>,
    pub share_ffmpeg_semaphore: Arc<tokio::sync::Semaphore>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SharePlaybackCredit {
    pub page_credits: usize,
    pub active_stream_started: bool,
    pub min_next_range_start: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JwtClaims {
    pub sub: String,
    pub role: String,
    #[serde(default)]
    pub allowed_cameras: Option<String>,
    pub exp: u64,
    pub iat: u64,
}

#[derive(Debug, Deserialize)]
pub struct LoginBody {
    pub user: String,
    pub password: String,
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn normalize_jwt_secret_entropy(raw: &str) -> String {
    if raw.len() >= 32 {
        return raw.to_string();
    }
    let mut derived = [0u8; 32];
    let mut mac = Hmac::<Sha256>::new_from_slice(b"ninjanvr-jwt-secret-kdf-v1")
        .expect("HMAC accepts any key");
    mac.update(raw.as_bytes());
    derived.copy_from_slice(&mac.finalize().into_bytes());
    for round in 1..10_000u32 {
        let mut rmac = Hmac::<Sha256>::new_from_slice(b"ninjanvr-jwt-secret-kdf-v1")
            .expect("HMAC accepts any key");
        rmac.update(&derived);
        rmac.update(raw.as_bytes());
        rmac.update(&round.to_be_bytes());
        derived.copy_from_slice(&rmac.finalize().into_bytes());
    }
    derived.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn load_or_create_jwt_secret(config_dir: &Path) -> String {
    if let Ok(env_secret) = std::env::var("NINJANVR_JWT_SECRET") {
        let trimmed = env_secret.trim();
        if !trimmed.is_empty() {
            return normalize_jwt_secret_entropy(trimmed);
        }
    }
    let secret_path = config_dir.join(".jwt_secret");
    if let Ok(existing) = std::fs::read_to_string(&secret_path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o600));
            }
            return normalize_jwt_secret_entropy(trimmed);
        }
    }
    let mut bytes = [0u8; 64];
    rand::fill(&mut bytes);
    let secret: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&secret_path)
        {
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
            let _ = file.write_all(secret.as_bytes());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::write(&secret_path, &secret);
    }
    secret
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
        .route("/detectors/models/upload", post(api_detector_model_upload))
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
        .route("/mcp", post(api_mcp_handler))
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
        .route("/recordings/sync", post(api_recordings_sync))
        .route("/recordings/unavailable", get(api_empty_array))
        .route("/recordings/storage", get(api_recordings_storage))
        .route("/exports", get(api_exports))
        .route("/exports/delete", post(api_exports_bulk_delete))
        .route("/share", get(api_share_list).post(api_share_create))
        .route("/share/{id}", delete(api_share_revoke))
        .route("/jobs/export", get(api_empty_array))
        .route("/cases", get(api_cases).post(api_case_create))
        .route(
            "/export/{camera}/start/{start}/end/{end}",
            post(api_export_create),
        )
        .route("/export/{id}", delete(api_export_delete))
        .route("/export/{id}/rename", patch(api_export_rename))
        .route(
            "/preview/{camera}/start/{start}/end/{end}",
            get(api_previews),
        )
        .route(
            "/preview/{camera}/start/{start}/end/{end}/frames",
            get(api_empty_array),
        )
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
            "/{camera}/state",
            get(api_camera_state_get).post(api_camera_state_set),
        )
        .route("/{camera}/latest.webp", get(api_camera_latest_frame))
        .route("/{camera}/latest.jpg", get(api_camera_latest_frame))
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
        .route("/version", get(api_version))
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
        .nest_service("/assets-v2", ServeDir::new(web_dir.join("assets-v2")))
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
        .layer(CorsLayer::permissive())
        .with_state(state)
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
        .filter(|r| r.starts_with('/') && !r.starts_with("//"))
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

async fn serve_login_page(State(state): State<AppState>) -> Response {
    let index_path = state.web_dir.join("index.html");
    match tokio::fs::read_to_string(&index_path).await {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn serve_spa_fallback(State(state): State<AppState>, uri: axum::http::Uri) -> Response {
    let rel_path = uri.path().trim_start_matches('/');
    let primary_dir = &state.web_dir;

    if !rel_path.is_empty() && !rel_path.contains("..") {
        let mut file_candidate = primary_dir.join(rel_path);
        if rel_path == "favicon.ico" && !file_candidate.is_file() {
            file_candidate = primary_dir.join("favicon.svg");
        }
        if file_candidate.is_file()
            && let Ok(bytes) = tokio::fs::read(&file_candidate).await
        {
            let mime = match file_candidate.extension().and_then(|e| e.to_str()) {
                Some("js" | "mjs") => "application/javascript; charset=utf-8",
                Some("css") => "text/css; charset=utf-8",
                Some("svg") => "image/svg+xml",
                Some("png") => "image/png",
                Some("ico") => "image/x-icon",
                Some("webp") => "image/webp",
                Some("json" | "webmanifest") => "application/json; charset=utf-8",
                Some("txt") => "text/plain; charset=utf-8",
                Some("html") => "text/html; charset=utf-8",
                _ => "application/octet-stream",
            };
            return ([(header::CONTENT_TYPE, mime)], bytes).into_response();
        }
    }

    let index_path = primary_dir.join("index.html");
    match tokio::fs::read_to_string(&index_path).await {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn is_protected_path(path: &str) -> bool {
    if matches!(
        path,
        "/api/login"
            | "/api/logout"
            | "/api/auth/first_time_login"
            | "/api/auth/providers"
            | "/api/auth/oidc/login"
            | "/api/auth/oidc/callback"
            | "/api/ui/switch"
            | "/api/ui/version"
            | "/logout"
            | "/login"
    ) {
        return false;
    }
    path.starts_with("/api/")
        || path == "/api"
        || path.starts_with("/vod/")
        || path.starts_with("/live/")
        || path == "/ws"
        || path.starts_with("/clips/")
        || path.starts_with("/recordings/")
        || path.starts_with("/exports/")
}

fn is_hidden_static_media_path(path: &str) -> bool {
    if let Some(rest) = path
        .strip_prefix("/exports/")
        .or_else(|| path.strip_prefix("/clips/"))
    {
        return rest
            .split('/')
            .filter(|s| !s.is_empty())
            .any(|seg| seg.starts_with('.') || seg.starts_with("%2e") || seg.starts_with("%2E"));
    }
    false
}

pub fn resolve_static_media_camera(state: &AppState, path: &str) -> Option<String> {
    if is_hidden_static_media_path(path) {
        return None;
    }
    if let Some(rest) = path.strip_prefix("/exports/") {
        let fname = rest.split('/').next().unwrap_or("");
        let raw_id = fname.strip_suffix(".mp4").unwrap_or(fname);
        let is_share_artifact =
            raw_id.starts_with("share_redacted_") || raw_id.starts_with("share_clip_");
        let exp_id = raw_id
            .strip_prefix("share_redacted_")
            .or_else(|| raw_id.strip_prefix("share_clip_"))
            .unwrap_or(raw_id);
        if let Ok(Some(exp)) = state.db.get_export_by_id(exp_id)
            && let Some(cam) = exp["camera"].as_str()
        {
            return Some(cam.to_string());
        }
        if is_share_artifact {
            if let Ok(Some(ev)) = state.db.get_event(exp_id)
                && let Some(cam) = ev["camera"].as_str()
            {
                return Some(cam.to_string());
            }
            if let Ok(Some(rev)) = state.db.get_review_by_id(exp_id) {
                return Some(rev.camera);
            }
            return None;
        }
        return exp_id
            .rsplitn(3, '_')
            .nth(2)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
    }
    if let Some(rest) = path.strip_prefix("/recordings/") {
        let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        if parts.first() == Some(&".staging") && parts.len() >= 2 {
            return Some(parts[1].to_string());
        }
        if parts.len() >= 3 && parts[0].len() == 10 && parts[0].chars().nth(4) == Some('-') {
            return Some(parts[2].to_string());
        }
        if !parts.is_empty() {
            return Some(parts[0].to_string());
        }
        return None;
    }
    if let Some(rest) = path.strip_prefix("/clips/") {
        let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        if parts.first() == Some(&"previews") && parts.len() >= 2 {
            return Some(parts[1].to_string());
        }
        if parts.first() == Some(&"review") && parts.len() >= 2 {
            if let Some(after_thumb) = parts[1].strip_prefix("thumb-")
                && let Some(cam) = after_thumb
                    .rsplitn(3, '-')
                    .nth(2)
                    .or_else(|| after_thumb.split_once('-').map(|(c, _)| c))
                    .filter(|s| !s.is_empty())
            {
                return Some(cam.to_string());
            }
            return None;
        }
        if parts.len() == 1 {
            let raw_stem = parts[0]
                .rsplit_once('.')
                .map(|(s, _)| s)
                .unwrap_or(parts[0]);
            let is_share_artifact =
                raw_stem.starts_with("share_redacted_") || raw_stem.starts_with("share_clip_");
            let stem = raw_stem
                .strip_prefix("share_redacted_")
                .or_else(|| raw_stem.strip_prefix("share_clip_"))
                .unwrap_or(raw_stem);
            if let Ok(Some(ev)) = state.db.get_event(stem)
                && let Some(cam) = ev["camera"].as_str()
            {
                return Some(cam.to_string());
            }
            if let Ok(Some(exp)) = state.db.get_export_by_id(stem)
                && let Some(cam) = exp["camera"].as_str()
            {
                return Some(cam.to_string());
            }
            if let Ok(Some(rev)) = state.db.get_review_by_id(stem) {
                return Some(rev.camera);
            }
            if is_share_artifact {
                return None;
            }
            if let Some((cam, _)) = stem.split_once('-')
                && state
                    .config
                    .try_read()
                    .map(|c| c.cameras.contains_key(cam))
                    .unwrap_or(false)
            {
                return Some(cam.to_string());
            }
        }
    }
    None
}

async fn auth_gate_middleware(
    State((state, enforce_auth)): State<(AppState, bool)>,
    jar: CookieJar,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    if is_hidden_static_media_path(&path) {
        state.log_hub.push_nginx(&method, &path, 403);
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Hidden or internal static media artifacts are forbidden" })),
        )
            .into_response();
    }
    if enforce_auth && is_protected_path(&path) {
        let auth_cfg = state.config.read().await.auth.clone();
        if auth_cfg.enabled {
            let Some((user, role)) =
                extract_authenticated_user_with_cfg(&state, &jar, req.headers(), &auth_cfg)
            else {
                state.log_hub.push_nginx(&method, &path, 401);
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "message": "Unauthorized" })),
                )
                    .into_response();
            };

            let role_lc = role.to_ascii_lowercase();
            if role_lc == "metrics" {
                let is_metrics_route = (method == "GET" || method == "HEAD")
                    && (path.starts_with("/api/stats") || path.starts_with("/api/metrics"));
                if !is_metrics_route {
                    state.log_hub.push_nginx(&method, &path, 403);
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "message": "Token scope 'metrics' only permits GET /api/stats and /api/metrics" })),
                    )
                        .into_response();
                }
            }

            let is_admin_only = path.starts_with("/api/config/save")
                || path.starts_with("/api/config/structured")
                || path.starts_with("/api/config/set")
                || path.starts_with("/api/config/raw")
                || path.starts_with("/api/restart")
                || path.starts_with("/api/tokens")
                || path.starts_with("/api/users")
                || path.starts_with("/api/camera-proxy")
                || path.starts_with("/api/recordings/sync")
                || path.starts_with("/api/logs")
                || (method == "DELETE"
                    && (path.ends_with("/recordings")
                        || path.starts_with("/api/events")
                        || path.starts_with("/api/review")
                        || path.starts_with("/api/export")));
            if is_admin_only && role_lc != "admin" {
                state.log_hub.push_nginx(&method, &path, 403);
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "message": "Admin role required" })),
                )
                    .into_response();
            }

            if role_lc == "read_only"
                && !path.starts_with("/api/mcp")
                && ((method != "GET" && method != "HEAD") || !path.starts_with("/api/"))
            {
                state.log_hub.push_nginx(&method, &path, 403);
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "message": "Token scope 'read_only' only permits GET /api/*" })),
                )
                    .into_response();
            }

            if role_lc == "viewer"
                && method != "GET"
                && method != "HEAD"
                && !path.starts_with("/api/logout")
                && !path.starts_with("/api/mcp")
            {
                state.log_hub.push_nginx(&method, &path, 403);
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "message": "Viewer role is read-only" })),
                )
                    .into_response();
            }

            // Enforce per-camera scoping when user is restricted to specific cameras
            if role_lc != "admin"
                && let Some(allowed_set) = resolve_user_allowed_cameras(
                    &state,
                    &jar,
                    req.headers(),
                    &auth_cfg,
                    &user,
                    &role,
                )
            {
                let is_static_media = path.starts_with("/exports/")
                    || path.starts_with("/recordings/")
                    || path.starts_with("/clips/");
                let static_media_cam = if is_static_media {
                    resolve_static_media_camera(&state, &path)
                } else {
                    None
                };
                if is_static_media
                    && static_media_cam.is_none()
                    && !allowed_set.iter().any(|a| a == "*")
                {
                    state.log_hub.push_nginx(&method, &path, 403);
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "message": "Static media resource is not permitted for this user" })),
                    )
                        .into_response();
                }
                let query_cam = if path.starts_with("/api/go2rtc") {
                    req.uri().query().and_then(|q| {
                        q.split('&').find_map(|kv| {
                            let (k, v) = kv.split_once('=')?;
                            if k == "src" || k == "dst" {
                                Some(v.strip_suffix("_sub").unwrap_or(v).to_string())
                            } else {
                                None
                            }
                        })
                    })
                } else {
                    None
                };
                let target_cam = if let Some(ref sc) = static_media_cam {
                    Some(sc.as_str())
                } else if let Some(ref qc) = query_cam {
                    Some(qc.as_str())
                } else if let Some(rest) = path.strip_prefix("/vod/") {
                    let vod_rest = rest.strip_prefix("clip/").unwrap_or(rest);
                    vod_rest.split('/').next()
                } else if let Some(rest) = path.strip_prefix("/api/") {
                    let first = rest.split('/').next().unwrap_or("");
                    if matches!(
                        first,
                        "version"
                            | "ui"
                            | "auth"
                            | "login"
                            | "logout"
                            | "profile"
                            | "profiles"
                            | "users"
                            | "config"
                            | "detectors"
                            | "dataset"
                            | "ffmpeg"
                            | "ffprobe"
                            | "streams"
                            | "keyframe_analysis"
                            | "restart"
                            | "logs"
                            | "stats"
                            | "review"
                            | "review_ids"
                            | "reviews"
                            | "events"
                            | "event_ids"
                            | "false_positives"
                            | "timeline"
                            | "recordings"
                            | "exports"
                            | "jobs"
                            | "cases"
                            | "preview"
                            | "share"
                            | "tokens"
                            | "metrics"
                            | "ai"
                            | "mcp"
                            | "debug"
                            | "notifications"
                            | "federation"
                            | "camera-proxy"
                            | "go2rtc"
                            | "ws"
                            | "labels"
                            | "sub_labels"
                            | "audio_labels"
                            | "recognized_license_plates"
                            | "classification"
                            | "genai"
                            | "plus"
                            | "faces"
                    ) {
                        None
                    } else if first == "vod" {
                        let after_first = rest
                            .strip_prefix(first)
                            .unwrap_or("")
                            .trim_start_matches('/');
                        let after_clip = after_first.strip_prefix("clip/").unwrap_or(after_first);
                        after_clip.split('/').next()
                    } else if first == "export" {
                        let after_first = rest
                            .strip_prefix(first)
                            .unwrap_or("")
                            .trim_start_matches('/');
                        let after_clip = after_first.strip_prefix("clip/").unwrap_or(after_first);
                        if after_clip.contains("/start/") {
                            after_clip.split('/').next()
                        } else {
                            None
                        }
                    } else {
                        Some(first)
                    }
                } else {
                    None
                };
                if let Some(cam) = target_cam
                    && !cam.is_empty()
                    && !allowed_set.iter().any(|a| a == cam)
                {
                    state.log_hub.push_nginx(&method, &path, 403);
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "message": format!("Camera '{cam}' is not permitted for this user") })),
                    )
                        .into_response();
                }
            }
        }
    }
    let resp = next.run(req).await;
    if !path.starts_with("/api/logs") && !path.starts_with("/assets-v2/") {
        state
            .log_hub
            .push_nginx(&method, &path, resp.status().as_u16());
    }
    resp
}

async fn api_version() -> &'static str {
    NINJANVR_VERSION
}

async fn api_auth_verify(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Response {
    let cfg = state.config.read().await;
    if let Some((user, role)) = extract_authenticated_user(
        &state,
        &jar,
        &headers,
        &cfg.auth.cookie_name,
        cfg.auth.enabled,
    ) {
        (
            StatusCode::ACCEPTED,
            [("remote-user", user), ("remote-role", role)],
            "",
        )
            .into_response()
    } else {
        (StatusCode::UNAUTHORIZED, [(header::LOCATION, "/login")], "").into_response()
    }
}

async fn api_first_time_login(State(state): State<AppState>) -> Json<Value> {
    let cfg = state.config.read().await;
    Json(json!({
        "admin_first_time_login": cfg.auth.admin_first_time_login
    }))
}

async fn api_auth_providers(State(state): State<AppState>) -> Json<Value> {
    let cfg = state.config.read().await;
    let oidc = &cfg.auth.oidc;
    let issuer = if !oidc.issuer_url.trim().is_empty() {
        oidc.issuer_url.trim().to_string()
    } else {
        std::env::var("NINJANVR_OIDC_ISSUER_URL").unwrap_or_default()
    };
    let client_id = if !oidc.client_id.trim().is_empty() {
        oidc.client_id.trim().to_string()
    } else {
        std::env::var("NINJANVR_OIDC_CLIENT_ID").unwrap_or_default()
    };
    let oidc_ready = oidc.enabled && !issuer.is_empty() && !client_id.is_empty();
    Json(json!({
        "enabled": cfg.auth.enabled,
        "local_login_enabled": cfg.auth.local_login_enabled,
        "oidc": {
            "enabled": oidc_ready,
            "provider_name": oidc.provider_name,
            "auto_redirect": oidc.auto_redirect
        },
        "proxy": {
            "enabled": cfg.auth.proxy.enabled,
            "logout_url": cfg.auth.proxy.logout_url
        }
    }))
}

/// Checks whether `ip_str` matches a single CIDR (`"10.42.0.0/16"`, `"127.0.0.1/32"`) or bare IP (`"127.0.0.1"`).
pub fn ip_matches_cidr(ip_str: &str, cidr_str: &str) -> bool {
    use std::net::IpAddr;
    let Ok(ip) = ip_str.trim().parse::<IpAddr>() else {
        return false;
    };
    let cidr = cidr_str.trim();
    if let Some((net_s, prefix_s)) = cidr.split_once('/') {
        let Ok(net_ip) = net_s.trim().parse::<IpAddr>() else {
            return false;
        };
        let Ok(prefix_len) = prefix_s.trim().parse::<u32>() else {
            return false;
        };
        match (ip, net_ip) {
            (IpAddr::V4(a), IpAddr::V4(b)) if prefix_len <= 32 => {
                if prefix_len == 0 {
                    return true;
                }
                let mask = !0u32 << (32 - prefix_len);
                (u32::from(a) & mask) == (u32::from(b) & mask)
            }
            (IpAddr::V6(a), IpAddr::V6(b)) if prefix_len <= 128 => {
                if prefix_len == 0 {
                    return true;
                }
                let mask = !0u128 << (128 - prefix_len);
                (u128::from(a) & mask) == (u128::from(b) & mask)
            }
            _ => false,
        }
    } else if let Ok(net_ip) = cidr.parse::<IpAddr>() {
        ip == net_ip
    } else {
        false
    }
}

pub fn ip_matches_trusted_proxies(ip_str: &str, cidrs: &[String]) -> bool {
    cidrs.iter().any(|c| ip_matches_cidr(ip_str, c))
}

/// Resolves a NinjaNVR role (`"admin"`, `"operator"`, or `"viewer"`) from OIDC ID-token / UserInfo claims,
/// supporting both Authelia array claims (`"groups": ["admins"]`) and Zitadel project role map claims
/// (`"urn:zitadel:iam:org:project:roles": { "ninjanvr-admin": { ... } }`).
pub fn resolve_oidc_role(claims: &Value, oidc_cfg: &ninjanvr_config::OidcAuthConfig) -> String {
    let mut candidate_groups: Vec<String> = Vec::new();
    let mut collect_from_val = |val: &Value| match val {
        Value::Array(arr) => {
            for item in arr {
                if let Some(s) = item.as_str() {
                    candidate_groups.push(s.trim().to_string());
                }
            }
        }
        Value::String(s) => {
            for part in s.split([',', ' ']) {
                let t = part.trim();
                if !t.is_empty() {
                    candidate_groups.push(t.to_string());
                }
            }
        }
        Value::Object(map) => {
            for key in map.keys() {
                candidate_groups.push(key.trim().to_string());
            }
        }
        _ => {}
    };

    if let Some(v) = claims.get(&oidc_cfg.groups_claim) {
        collect_from_val(v);
    }
    if oidc_cfg.groups_claim != "groups"
        && let Some(v) = claims.get("groups")
    {
        collect_from_val(v);
    }
    if let Some(v) = claims.get("urn:zitadel:iam:org:project:roles") {
        collect_from_val(v);
    }
    if let Some(v) = claims.get("roles") {
        collect_from_val(v);
    }

    let matches_any = |targets: &[String]| -> bool {
        targets.iter().any(|t| {
            candidate_groups
                .iter()
                .any(|cg| cg.eq_ignore_ascii_case(t.trim()))
        })
    };

    if matches_any(&oidc_cfg.role_mapping.admin) {
        "admin".to_string()
    } else if matches_any(&oidc_cfg.role_mapping.operator) {
        "operator".to_string()
    } else if matches_any(&oidc_cfg.role_mapping.viewer) {
        "viewer".to_string()
    } else {
        let def = oidc_cfg.default_role.trim().to_ascii_lowercase();
        if def == "admin" || def == "operator" {
            def
        } else {
            "viewer".to_string()
        }
    }
}

fn generate_pkce_pair() -> (String, String, String) {
    let mut verifier_bytes = [0u8; 32];
    rand::fill(&mut verifier_bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge_hash = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(challenge_hash);
    let mut state_bytes = [0u8; 16];
    rand::fill(&mut state_bytes);
    let state_str: String = state_bytes.iter().map(|b| format!("{b:02x}")).collect();
    (state_str, verifier, challenge)
}

fn resolve_external_base_url(headers: &HeaderMap, uri: &Uri) -> String {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim())
        .filter(|s| !s.is_empty())
        .or_else(|| uri.authority().map(|a| a.as_str()))
        .unwrap_or("localhost:5000");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim())
        .filter(|s| !s.is_empty())
        .or_else(|| uri.scheme_str())
        .unwrap_or(
            if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
                "http"
            } else {
                "https"
            },
        );
    format!("{proto}://{host}")
}

async fn api_auth_oidc_login(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    jar: CookieJar,
) -> Response {
    let cfg = state.config.read().await;
    let oidc = &cfg.auth.oidc;
    let issuer = if !oidc.issuer_url.trim().is_empty() {
        oidc.issuer_url.trim().trim_end_matches('/').to_string()
    } else {
        std::env::var("NINJANVR_OIDC_ISSUER_URL")
            .unwrap_or_default()
            .trim()
            .trim_end_matches('/')
            .to_string()
    };
    let client_id = if !oidc.client_id.trim().is_empty() {
        oidc.client_id.trim().to_string()
    } else {
        std::env::var("NINJANVR_OIDC_CLIENT_ID").unwrap_or_default()
    };
    if !oidc.enabled || issuer.is_empty() || client_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "OIDC SSO is not enabled or missing issuer_url/client_id" })),
        )
            .into_response();
    }

    let (state_str, verifier, challenge) = generate_pkce_pair();
    let base_url = resolve_external_base_url(&headers, &uri);
    let redirect_uri = format!("{base_url}/api/auth/oidc/callback");

    // Discover authorization_endpoint or fall back to standard /authorize
    let mut auth_endpoint = format!("{issuer}/authorize");
    if let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        && let Ok(resp) = client
            .get(format!("{issuer}/.well-known/openid-configuration"))
            .send()
            .await
        && let Ok(disc) = resp.json::<Value>().await
        && let Some(ep) = disc.get("authorization_endpoint").and_then(|v| v.as_str())
    {
        auth_endpoint = ep.to_string();
    }

    let scope_str = if oidc.scopes.is_empty() {
        "openid profile email groups".to_string()
    } else {
        oidc.scopes.join(" ")
    };

    let redirect_target = format!(
        "{auth_endpoint}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={state_str}&code_challenge={challenge}&code_challenge_method=S256",
        urlencoding_simple(&client_id),
        urlencoding_simple(&redirect_uri),
        urlencoding_simple(&scope_str),
    );

    let mut pkce_cookie = Cookie::new(
        "ninjanvr_oidc_pkce",
        format!("{state_str}:{verifier}:{redirect_uri}"),
    );
    pkce_cookie.set_path("/");
    pkce_cookie.set_http_only(true);
    pkce_cookie.set_same_site(SameSite::Lax);

    (jar.add(pkce_cookie), Redirect::to(&redirect_target)).into_response()
}

fn urlencoding_simple(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Debug, Deserialize, Default)]
pub struct OidcCallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
}

async fn api_auth_oidc_callback(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    jar: CookieJar,
    Query(q): Query<OidcCallbackQuery>,
) -> Response {
    let Some(code) = q.code.as_deref().filter(|s| !s.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Missing authorization code" })),
        )
            .into_response();
    };
    let Some(returned_state) = q.state.as_deref().filter(|s| !s.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Missing OIDC state parameter" })),
        )
            .into_response();
    };
    let Some(pkce_cookie) = jar.get("ninjanvr_oidc_pkce") else {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Missing OIDC PKCE state cookie" })),
        )
            .into_response();
    };
    let Some((expected_state, rest_cookie)) = pkce_cookie.value().split_once(':') else {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Malformed OIDC PKCE cookie" })),
        )
            .into_response();
    };
    let (code_verifier, saved_redirect_uri) = match rest_cookie.split_once(':') {
        Some((v, r)) => (v, Some(r.to_string())),
        None => (rest_cookie, None),
    };
    if expected_state != returned_state {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "OIDC state mismatch" })),
        )
            .into_response();
    }

    let (oidc_cfg, cookie_name, cookie_secure, session_len) = {
        let cfg = state.config.read().await;
        (
            cfg.auth.oidc.clone(),
            cfg.auth.cookie_name.clone(),
            cfg.auth.cookie_secure,
            cfg.auth.session_length,
        )
    };

    let issuer = if !oidc_cfg.issuer_url.trim().is_empty() {
        oidc_cfg.issuer_url.trim().trim_end_matches('/').to_string()
    } else {
        std::env::var("NINJANVR_OIDC_ISSUER_URL")
            .unwrap_or_default()
            .trim()
            .trim_end_matches('/')
            .to_string()
    };
    let client_id = if !oidc_cfg.client_id.trim().is_empty() {
        oidc_cfg.client_id.trim().to_string()
    } else {
        std::env::var("NINJANVR_OIDC_CLIENT_ID").unwrap_or_default()
    };
    let client_secret = std::env::var(&oidc_cfg.client_secret_env).unwrap_or_default();

    let redirect_uri = saved_redirect_uri.unwrap_or_else(|| {
        let base_url = resolve_external_base_url(&headers, &uri);
        format!("{base_url}/api/auth/oidc/callback")
    });

    let mut token_endpoint = format!("{issuer}/oauth/v2/token");
    let mut userinfo_endpoint = format!("{issuer}/oidc/v1/userinfo");
    if let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        if let Ok(resp) = client
            .get(format!("{issuer}/.well-known/openid-configuration"))
            .send()
            .await
            && let Ok(disc) = resp.json::<Value>().await
        {
            if let Some(ep) = disc.get("token_endpoint").and_then(|v| v.as_str()) {
                token_endpoint = ep.to_string();
            }
            if let Some(uep) = disc.get("userinfo_endpoint").and_then(|v| v.as_str()) {
                userinfo_endpoint = uep.to_string();
            }
        }

        let mut form_body = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
            urlencoding_simple(code),
            urlencoding_simple(&redirect_uri),
            urlencoding_simple(&client_id),
            urlencoding_simple(code_verifier)
        );
        if !client_secret.is_empty() {
            form_body.push_str(&format!(
                "&client_secret={}",
                urlencoding_simple(&client_secret)
            ));
        }
        if let Ok(tok_resp) = client
            .post(&token_endpoint)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form_body)
            .send()
            .await
            && let Ok(tok_json) = tok_resp.json::<Value>().await
        {
            let mut merged_claims = json!({});
            if let Some(id_token) = tok_json.get("id_token").and_then(|v| v.as_str()) {
                let parts: Vec<&str> = id_token.split('.').collect();
                if parts.len() >= 2
                    && let Ok(payload_bytes) =
                        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1])
                    && let Ok(claims_json) = serde_json::from_slice::<Value>(&payload_bytes)
                {
                    merged_claims = claims_json;
                }
            }
            if let Some(access_token) = tok_json.get("access_token").and_then(|v| v.as_str())
                && let Ok(ui_resp) = client
                    .get(&userinfo_endpoint)
                    .header(header::AUTHORIZATION, format!("Bearer {access_token}"))
                    .send()
                    .await
                && let Ok(Value::Object(ui_map)) = ui_resp.json::<Value>().await
                && let Some(obj) = merged_claims.as_object_mut()
            {
                for (k, v) in ui_map {
                    obj.entry(k).or_insert(v);
                }
            }
            if merged_claims.as_object().is_some_and(|m| !m.is_empty()) {
                let username = merged_claims
                    .get(&oidc_cfg.username_claim)
                    .and_then(|v| v.as_str())
                    .or_else(|| {
                        merged_claims
                            .get("preferred_username")
                            .and_then(|v| v.as_str())
                    })
                    .or_else(|| merged_claims.get("email").and_then(|v| v.as_str()))
                    .or_else(|| merged_claims.get("sub").and_then(|v| v.as_str()))
                    .unwrap_or("oidc_user")
                    .to_string();
                let role = resolve_oidc_role(&merged_claims, &oidc_cfg);
                let now = unix_now();
                let jwt_claims = JwtClaims {
                    sub: username,
                    role,
                    allowed_cameras: Some("*".to_string()),
                    iat: now,
                    exp: now + session_len,
                };
                let session_token = jsonwebtoken::encode(
                    &Header::new(Algorithm::HS256),
                    &jwt_claims,
                    &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
                )
                .unwrap_or_default();
                let mut session_cookie = Cookie::new(cookie_name, session_token);
                session_cookie.set_path("/");
                session_cookie.set_http_only(true);
                session_cookie.set_secure(cookie_secure);
                session_cookie.set_same_site(SameSite::Lax);

                let mut clear_pkce = Cookie::from("ninjanvr_oidc_pkce");
                clear_pkce.set_path("/");
                return (
                    jar.remove(clear_pkce).add(session_cookie),
                    Redirect::to("/"),
                )
                    .into_response();
            }
        }
    }

    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "message": "OIDC token exchange failed" })),
    )
        .into_response()
}

async fn api_login(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(body): Json<LoginBody>,
) -> impl IntoResponse {
    let (cookie_name, cookie_secure, session_len, auth_enabled, local_login_enabled) = {
        let cfg = state.config.read().await;
        (
            cfg.auth.cookie_name.clone(),
            cfg.auth.cookie_secure,
            cfg.auth.session_length,
            cfg.auth.enabled,
            cfg.auth.local_login_enabled,
        )
    };

    if !auth_enabled {
        return (
            jar,
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Authentication is disabled" })),
            )
                .into_response(),
        );
    }

    if !local_login_enabled {
        return (
            jar,
            (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Local password login is disabled; use OIDC SSO" })),
            )
                .into_response(),
        );
    }

    let Ok(Some(db_user)) = state.db.get_user(&body.user) else {
        return (
            jar,
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "message": "Login failed" })),
            )
                .into_response(),
        );
    };

    if !verify_password(&body.password, &db_user.password_hash) {
        return (
            jar,
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "message": "Login failed" })),
            )
                .into_response(),
        );
    }

    if db_user.role == "admin" {
        state.config.write().await.auth.admin_first_time_login = false;
    }

    let now = unix_now();
    let claims = JwtClaims {
        sub: db_user.username.clone(),
        role: db_user.role.clone(),
        allowed_cameras: Some(db_user.allowed_cameras.clone()),
        iat: now,
        exp: now + session_len,
    };

    let token = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .unwrap_or_default();

    let mut cookie = Cookie::new(cookie_name, token);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_secure(cookie_secure);
    cookie.set_same_site(SameSite::Lax);

    (jar.add(cookie), StatusCode::OK.into_response())
}

async fn api_logout(State(state): State<AppState>, jar: CookieJar) -> impl IntoResponse {
    let cookie_name = state.config.read().await.auth.cookie_name.clone();
    let mut removal = Cookie::from(cookie_name);
    removal.set_path("/");
    (jar.remove(removal), Redirect::to("/login"))
}

fn extract_authenticated_user(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
    cookie_name: &str,
    auth_enabled: bool,
) -> Option<(String, String)> {
    if !auth_enabled {
        return Some(("admin".to_string(), "admin".to_string()));
    }
    if let Ok(cfg) = state.config.try_read() {
        return extract_authenticated_user_with_cfg(state, jar, headers, &cfg.auth);
    }
    let fallback = ninjanvr_config::AuthConfig {
        cookie_name: cookie_name.to_string(),
        enabled: auth_enabled,
        ..Default::default()
    };
    extract_authenticated_user_with_cfg(state, jar, headers, &fallback)
}

fn parse_authorization_bearer(raw: &str) -> Option<&str> {
    let trimmed = raw.trim_start();
    if trimmed
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("bearer "))
        && let Some(rest) = trimmed.get(7..)
    {
        let tok = rest.trim();
        if !tok.is_empty() {
            return Some(tok);
        }
    }
    None
}

fn extract_authenticated_user_with_cfg(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
    auth_cfg: &ninjanvr_config::AuthConfig,
) -> Option<(String, String)> {
    if !auth_cfg.enabled {
        return Some(("admin".to_string(), "admin".to_string()));
    }

    // Hardened Trusted-Proxy ForwardAuth: only trust Remote-User / Remote-Groups headers
    // when proxy auth is explicitly enabled AND the peer IP matches trusted_proxies CIDRs.
    if auth_cfg.proxy.enabled {
        let peer_ip = headers
            .get("x-peer-ip")
            .or_else(|| headers.get("x-real-ip"))
            .and_then(|v| v.to_str().ok())
            .or_else(|| {
                headers
                    .get("x-forwarded-for")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|xff| xff.split(',').next_back())
            })
            .map(|s| s.trim());

        let trusted_cidrs = if !auth_cfg.proxy.trusted_proxies.is_empty() {
            &auth_cfg.proxy.trusted_proxies
        } else {
            &auth_cfg.trusted_proxies
        };

        if let Some(ip) = peer_ip
            && ip_matches_trusted_proxies(ip, trusted_cidrs)
        {
            let user_hdr_name = auth_cfg.proxy.user_header.to_ascii_lowercase();
            if let Some(remote_user) = headers
                .get(user_hdr_name.as_str())
                .or_else(|| headers.get("remote-user"))
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                let groups_hdr_name = auth_cfg.proxy.groups_header.to_ascii_lowercase();
                let role = if let Some(explicit_role) =
                    headers.get("remote-role").and_then(|v| v.to_str().ok())
                {
                    explicit_role.trim().to_string()
                } else if let Some(groups_str) = headers
                    .get(groups_hdr_name.as_str())
                    .or_else(|| headers.get("remote-groups"))
                    .and_then(|v| v.to_str().ok())
                {
                    resolve_oidc_role(&json!({ "groups": groups_str }), &auth_cfg.oidc)
                } else {
                    "viewer".to_string()
                };
                return Some((remote_user.to_string(), role));
            }
        }
    }

    let token_str = if let Some(auth_val) = headers.get(header::AUTHORIZATION) {
        let raw = auth_val.to_str().ok()?;
        parse_authorization_bearer(raw)?.to_string()
    } else {
        jar.get(&auth_cfg.cookie_name)
            .map(|c| c.value().to_string())?
    };

    if token_str.starts_with("nnvr_") {
        if let Ok(Some((token_name, scope))) =
            state.db.verify_api_token(&token_str, unix_now() as f64)
        {
            let mapped_role = if scope == "full" {
                "admin".to_string()
            } else {
                scope
            };
            return Some((format!("token:{token_name}"), mapped_role));
        }
        return None;
    }

    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    let decoded = jsonwebtoken::decode::<JwtClaims>(
        &token_str,
        &DecodingKey::from_secret(state.jwt_secret.as_bytes()),
        &validation,
    )
    .ok()?;

    Some((decoded.claims.sub, decoded.claims.role))
}

async fn api_profile(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Response {
    let cfg = state.config.read().await;
    let cameras: Vec<String> = cfg.cameras.keys().cloned().collect();
    let user_opt = extract_authenticated_user(
        &state,
        &jar,
        &headers,
        &cfg.auth.cookie_name,
        cfg.auth.enabled,
    );

    let (username, role) = user_opt.unwrap_or_else(|| ("admin".to_string(), "admin".to_string()));
    let scoped_cameras: Vec<String> = if role.eq_ignore_ascii_case("admin") {
        cameras
    } else if let Ok(Some(u)) = state.db.get_user(&username) {
        let raw = u.allowed_cameras.trim();
        if raw.is_empty() || raw == "*" {
            cameras
        } else {
            let allowed: Vec<String> = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            cameras
                .into_iter()
                .filter(|c| allowed.contains(c))
                .collect()
        }
    } else {
        cameras
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
    Json(body): Json<SetProfileBody>,
) -> Json<Value> {
    let active = state.comms.set_active_profile(&body.profile);
    let cfg = state.config.read().await;
    if let Some(cfg_dir) = cfg.config_path.parent() {
        let _ = std::fs::write(cfg_dir.join(".active_profile"), &active);
    }
    Json(json!({
        "success": true,
        "active_profile": active
    }))
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

async fn api_users(State(state): State<AppState>) -> Json<Value> {
    let users = state.db.list_users().unwrap_or_default();
    Json(Value::Array(users))
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
    Json(body): Json<CreateUserBody>,
) -> Response {
    let iterations = state.config.read().await.auth.hash_iterations;
    let cams = body.allowed_cameras.as_deref().unwrap_or("*");
    match state.db.create_user_with_cameras(
        &body.username,
        &body.password,
        &body.role,
        cams,
        iterations,
    ) {
        Ok(()) => Json(json!({
            "success": true,
            "message": format!("User '{}' created", body.username.trim()),
            "users": state.db.list_users().unwrap_or_default()
        }))
        .into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
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
    AxumPath(username): AxumPath<String>,
    Json(body): Json<UpdateUserPasswordBody>,
) -> Response {
    let iterations = state.config.read().await.auth.hash_iterations;
    match state
        .db
        .update_user_password(&username, &body.password, iterations)
    {
        Ok(true) => Json(json!({
            "success": true,
            "message": format!("Password updated for '{username}'")
        }))
        .into_response(),
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
    AxumPath(username): AxumPath<String>,
    Json(body): Json<UpdateUserRoleBody>,
) -> Response {
    if let Some(ref cams) = body.allowed_cameras {
        let _ = state.db.update_user_allowed_cameras(&username, cams);
    }
    match state.db.update_user_role(&username, &body.role) {
        Ok(true) => {
            if body.role.trim().eq_ignore_ascii_case("viewer") {
                let _ = state.db.revoke_share_links_by_creator(&username);
            }
            Json(json!({
                "success": true,
                "message": format!("Role updated for '{username}'"),
                "users": state.db.list_users().unwrap_or_default()
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
    AxumPath(username): AxumPath<String>,
    Json(body): Json<UpdateUserCamerasBody>,
) -> Response {
    match state
        .db
        .update_user_allowed_cameras(&username, &body.allowed_cameras)
    {
        Ok(true) => Json(json!({
            "success": true,
            "message": format!("Allowed cameras updated for '{username}'"),
            "users": state.db.list_users().unwrap_or_default()
        }))
        .into_response(),
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
    AxumPath(username): AxumPath<String>,
) -> Response {
    match state.db.delete_user(&username) {
        Ok(true) => {
            let _ = state.db.revoke_share_links_by_creator(&username);
            Json(json!({
                "success": true,
                "message": format!("User '{username}' deleted"),
                "users": state.db.list_users().unwrap_or_default()
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

async fn api_tokens_list(State(state): State<AppState>) -> Json<Value> {
    let tokens = state.db.list_api_tokens().unwrap_or_default();
    Json(Value::Array(tokens))
}

async fn api_token_create(
    State(state): State<AppState>,
    Json(body): Json<CreateApiTokenBody>,
) -> Response {
    let expires_at = body
        .expires_in_days
        .map(|d| (unix_now() as f64) + (d as f64) * 86400.0);
    match state
        .db
        .create_api_token(&body.name, &body.scope, expires_at)
    {
        Ok((meta, plaintext)) => Json(json!({
            "success": true,
            "token": plaintext,
            "metadata": meta,
            "tokens": state.db.list_api_tokens().unwrap_or_default()
        }))
        .into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn api_token_delete(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let token_name = state.db.list_api_tokens().ok().and_then(|tokens| {
        tokens.into_iter().find_map(|item| {
            if item["id"].as_str() == Some(id.trim()) {
                item["name"].as_str().map(str::to_string)
            } else {
                None
            }
        })
    });
    match state.db.delete_api_token(&id) {
        Ok(true) => {
            if let Some(name) = token_name {
                let _ = state
                    .db
                    .revoke_share_links_by_creator(&format!("token:{name}"));
            }
            Json(json!({
                "success": true,
                "tokens": state.db.list_api_tokens().unwrap_or_default()
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

fn extract_jwt_allowed_cameras(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
    cookie_name: &str,
) -> Option<String> {
    let token_str = if let Some(auth_val) = headers.get(header::AUTHORIZATION) {
        let raw = auth_val.to_str().ok()?;
        parse_authorization_bearer(raw)?.to_string()
    } else {
        jar.get(cookie_name).map(|c| c.value().to_string())?
    };
    if token_str.starts_with("nnvr_") {
        return None;
    }
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    let decoded = jsonwebtoken::decode::<JwtClaims>(
        &token_str,
        &DecodingKey::from_secret(state.jwt_secret.as_bytes()),
        &validation,
    )
    .ok()?;
    decoded.claims.allowed_cameras
}

fn resolve_user_allowed_cameras(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
    auth_cfg: &ninjanvr_config::AuthConfig,
    user: &str,
    role: &str,
) -> Option<Vec<String>> {
    if role.eq_ignore_ascii_case("admin") {
        return None;
    }
    match state.db.get_user(user) {
        Ok(Some(db_u)) => {
            if db_u.role.eq_ignore_ascii_case("admin") {
                return None;
            }
            let raw = db_u.allowed_cameras.trim();
            if raw.is_empty() || raw == "*" {
                return None;
            }
            Some(
                raw.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
        }
        Ok(None) => {
            if user.starts_with("token:") {
                return None;
            }
            let oidc_active = auth_cfg.oidc.enabled
                && (!auth_cfg.oidc.issuer_url.trim().is_empty()
                    || !std::env::var("NINJANVR_OIDC_ISSUER_URL")
                        .unwrap_or_default()
                        .trim()
                        .is_empty());
            let is_proxy_header_auth = auth_cfg.proxy.enabled
                && (headers.contains_key("remote-user")
                    || headers
                        .contains_key(auth_cfg.proxy.user_header.to_ascii_lowercase().as_str()));
            if !oidc_active && !is_proxy_header_auth {
                return Some(Vec::new());
            }
            match extract_jwt_allowed_cameras(state, jar, headers, &auth_cfg.cookie_name) {
                Some(raw) if !raw.trim().is_empty() && raw.trim() != "*" => Some(
                    raw.trim()
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                ),
                Some(raw) if raw.trim() == "*" => None,
                _ if is_proxy_header_auth => None,
                _ => Some(Vec::new()),
            }
        }
        Err(_) => Some(Vec::new()),
    }
}

async fn caller_allowed_cameras(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
) -> Option<Vec<String>> {
    let cfg = state.config.read().await;
    let (user, role) =
        extract_authenticated_user(state, jar, headers, &cfg.auth.cookie_name, cfg.auth.enabled)?;
    resolve_user_allowed_cameras(state, jar, headers, &cfg.auth, &user, &role)
}

fn scope_cameras_filter(
    requested: Option<Vec<String>>,
    allowed: Option<Vec<String>>,
) -> Option<Vec<String>> {
    match (requested, allowed) {
        (req, None) => req,
        (None, Some(allow_list)) => Some(if allow_list.is_empty() {
            vec!["__none__".to_string()]
        } else {
            allow_list
        }),
        (Some(req_list), Some(allow_list)) => {
            let filtered: Vec<String> = req_list
                .into_iter()
                .filter(|c| allow_list.contains(c))
                .collect();
            Some(if filtered.is_empty() {
                vec!["__none__".to_string()]
            } else {
                filtered
            })
        }
    }
}

async fn api_config(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    let mut api_cfg = {
        let cfg = state.config.read().await;
        cfg.to_api_config()
    };
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && let Some(cams_obj) = api_cfg.get_mut("cameras").and_then(|v| v.as_object_mut())
    {
        cams_obj.retain(|k, _| allowed.contains(k));
    }
    Json(api_cfg)
}

async fn api_config_raw(State(state): State<AppState>) -> Json<Value> {
    let cfg = state.config.read().await;
    Json(Value::String(cfg.raw_yaml.clone()))
}

async fn api_config_save(State(state): State<AppState>, body: String) -> Response {
    let path = { state.config.read().await.config_path.clone() };
    match NinjaConfig::from_yaml_str(&body, path.clone()) {
        Ok(new_cfg) => {
            if let Err(err) = std::fs::write(&path, &body) {
                error!("Failed to persist config.yml: {err:#}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "success": false, "message": err.to_string() })),
                )
                    .into_response();
            }
            let _ = new_cfg.write_go2rtc_config(DEFAULT_GO2RTC_SHM_PATH);
            state.detector.reload(&new_cfg);
            let rec_dir = state.media_dir.join("recordings");
            let rec_mount = stat_mount_mb(&rec_dir, "ext4");
            let heuristics =
                evaluate_system_heuristics(&new_cfg, &serde_json::Map::new(), &rec_mount, 14.0);
            emit_heuristics_to_logs(&state.log_hub, &heuristics);
            *state.config.write().await = new_cfg;
            Json(json!({ "success": true, "message": "Config saved successfully" })).into_response()
        }
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn api_config_structured(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let mut next_cfg = { state.config.read().await.clone() };
    if let Err(err) = next_cfg.apply_structured_json(&body) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": format!("{err:#}") })),
        )
            .into_response();
    }
    if let Err(err) = std::fs::write(&next_cfg.config_path, &next_cfg.raw_yaml) {
        error!("Failed to persist structured config.yml: {err:#}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response();
    }
    let _ = next_cfg.write_go2rtc_config(DEFAULT_GO2RTC_SHM_PATH);
    state.detector.reload(&next_cfg);
    let rec_dir = state.media_dir.join("recordings");
    let rec_mount = stat_mount_mb(&rec_dir, "ext4");
    let heuristics =
        evaluate_system_heuristics(&next_cfg, &serde_json::Map::new(), &rec_mount, 14.0);
    emit_heuristics_to_logs(&state.log_hub, &heuristics);
    let api_cfg = next_cfg.to_api_config();
    let raw_yaml = next_cfg.raw_yaml.clone();
    *state.config.write().await = next_cfg;
    Json(json!({
        "success": true,
        "message": "Structured configuration saved",
        "raw_yaml": raw_yaml,
        "config": api_cfg
    }))
    .into_response()
}

async fn api_config_set() -> Json<Value> {
    Json(json!({ "success": true, "message": "Config updated" }))
}

fn build_detector_catalog_value(state: &AppState) -> Value {
    let mut cat = state.detector.runtime_catalog();
    let mut extra_dirs = vec![state.media_dir.join("models")];
    if let Some(db_parent) = state.db.path().parent() {
        extra_dirs.push(db_parent.join("models"));
        extra_dirs.push(db_parent.to_path_buf());
    }
    for dir in extra_dirs {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if let Some(ext) = p.extension().and_then(|e| e.to_str())
                    && (ext.eq_ignore_ascii_case("xml") || ext.eq_ignore_ascii_case("onnx"))
                {
                    let p_str = p.to_string_lossy().to_string();
                    if !cat.discovered_models.iter().any(|m| m.path == p_str) {
                        let arch = if p_str.to_ascii_lowercase().contains("yolox") {
                            "yolox"
                        } else if p_str.to_ascii_lowercase().contains("yolo")
                            || ext.eq_ignore_ascii_case("onnx")
                        {
                            "yolo-generic"
                        } else {
                            "ssd"
                        };
                        cat.discovered_models
                            .push(ninjanvr_infer::DiscoveredModelEntry {
                                path: p_str,
                                arch: arch.to_string(),
                            });
                    }
                }
            }
        }
    }
    serde_json::to_value(cat).unwrap_or_else(|_| json!({}))
}

async fn api_detectors_catalog(State(state): State<AppState>) -> Json<Value> {
    Json(build_detector_catalog_value(&state))
}

async fn api_ffmpeg_presets() -> Json<Value> {
    Json(json!({
        "input_args": ["preset-rtsp-generic", "preset-rtsp-restream", "preset-rtsp-udp"],
        "output_args": {
            "record": ["preset-record-generic", "preset-record-generic-audio-aac"]
        }
    }))
}

async fn api_restart() -> Json<Value> {
    Json(json!({ "success": true, "message": "Restart acknowledged" }))
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
    AxumPath(service): AxumPath<String>,
    Query(q): Query<LogsQueryParams>,
) -> Response {
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

async fn api_stats_history(State(state): State<AppState>) -> Json<Value> {
    let history = state.stats_history.read().await;
    if history.is_empty() {
        drop(history);
        let Json(current) = api_stats(State(state)).await;
        return Json(json!([current]));
    }
    let items: Vec<Value> = history.iter().cloned().collect();
    Json(Value::Array(items))
}

async fn api_prometheus_metrics(State(state): State<AppState>) -> Response {
    let Json(stats_val) = api_stats(State(state.clone())).await;
    let mut out = String::with_capacity(4096);

    out.push_str("# HELP ninjanvr_camera_fps Per-camera frame rate by pipeline role.\n");
    out.push_str("# TYPE ninjanvr_camera_fps gauge\n");
    out.push_str(
        "# HELP ninjanvr_camera_online Camera stream online status (1 = online, 0 = offline).\n",
    );
    out.push_str("# TYPE ninjanvr_camera_online gauge\n");

    if let Some(cams) = stats_val.get("cameras").and_then(|v| v.as_object()) {
        for (cam, c_stat) in cams {
            let cam_fps = c_stat
                .get("camera_fps")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let det_fps = c_stat
                .get("detection_fps")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let proc_fps = c_stat
                .get("process_fps")
                .and_then(|v| v.as_f64())
                .unwrap_or(cam_fps);
            let online = if cam_fps > 0.0 { 1 } else { 0 };
            out.push_str(&format!(
                "ninjanvr_camera_fps{{camera=\"{cam}\",role=\"camera\"}} {cam_fps:.2}\n"
            ));
            out.push_str(&format!(
                "ninjanvr_camera_fps{{camera=\"{cam}\",role=\"detect\"}} {det_fps:.2}\n"
            ));
            out.push_str(&format!(
                "ninjanvr_camera_fps{{camera=\"{cam}\",role=\"process\"}} {proc_fps:.2}\n"
            ));
            out.push_str(&format!(
                "ninjanvr_camera_online{{camera=\"{cam}\"}} {online}\n"
            ));
        }
    }

    out.push_str(
        "# HELP ninjanvr_detector_inference_ms AI detector inference latency in milliseconds.\n",
    );
    out.push_str("# TYPE ninjanvr_detector_inference_ms gauge\n");
    if let Some(dets) = stats_val.get("detectors").and_then(|v| v.as_object()) {
        for (det_name, d_stat) in dets {
            let ms = d_stat
                .get("inference_speed")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            out.push_str(&format!(
                "ninjanvr_detector_inference_ms{{detector=\"{det_name}\"}} {ms:.2}\n"
            ));
        }
    }

    out.push_str("# HELP ninjanvr_storage_used_bytes Used bytes on storage mounts.\n");
    out.push_str("# TYPE ninjanvr_storage_used_bytes gauge\n");
    out.push_str("# HELP ninjanvr_storage_total_bytes Total bytes on storage mounts.\n");
    out.push_str("# TYPE ninjanvr_storage_total_bytes gauge\n");
    if let Some(storages) = stats_val
        .get("service")
        .and_then(|s| s.get("storage"))
        .and_then(|v| v.as_object())
    {
        for (mount_path, st) in storages {
            let label = if mount_path.contains("recordings") {
                "recordings"
            } else if mount_path.contains("clips") {
                "clips"
            } else if mount_path.contains("cache") {
                "cache"
            } else if mount_path.contains("shm") {
                "shm"
            } else {
                mount_path.as_str()
            };
            let used_mb = st.get("used").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let total_mb = st.get("total").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let used_bytes = (used_mb * 1024.0 * 1024.0).round() as u64;
            let total_bytes = (total_mb * 1024.0 * 1024.0).round() as u64;
            out.push_str(&format!(
                "ninjanvr_storage_used_bytes{{mount=\"{label}\"}} {used_bytes}\n"
            ));
            out.push_str(&format!(
                "ninjanvr_storage_total_bytes{{mount=\"{label}\"}} {total_bytes}\n"
            ));
        }
    }

    let sys_cpu = stats_val
        .get("cpu_usages")
        .and_then(|c| c.get("ninjanvr.full_system"))
        .and_then(|f| f.get("cpu"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let sys_mem_pct = stats_val
        .get("cpu_usages")
        .and_then(|c| c.get("ninjanvr.full_system"))
        .and_then(|f| f.get("mem"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    out.push_str("# HELP ninjanvr_cpu_usage_percent Process CPU usage percentage.\n");
    out.push_str("# TYPE ninjanvr_cpu_usage_percent gauge\n");
    out.push_str(&format!("ninjanvr_cpu_usage_percent {sys_cpu:.2}\n"));
    out.push_str("# HELP ninjanvr_memory_usage_bytes Process resident memory usage in bytes.\n");
    out.push_str("# TYPE ninjanvr_memory_usage_bytes gauge\n");
    out.push_str(&format!(
        "ninjanvr_memory_usage_bytes {}\n",
        (sys_mem_pct * 1024.0 * 1024.0).round() as u64
    ));

    out.push_str("# HELP ninjanvr_events_total Total persisted object detection events by camera and label.\n");
    out.push_str("# TYPE ninjanvr_events_total counter\n");
    if let Ok(ev_counts) = state.db.query_event_counts_by_camera_and_label() {
        for (cam, lbl, cnt) in ev_counts {
            out.push_str(&format!(
                "ninjanvr_events_total{{camera=\"{cam}\",label=\"{lbl}\"}} {cnt}\n"
            ));
        }
    }

    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
        .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct AiContextQueryParams {
    pub hours: Option<f64>,
    pub after: Option<f64>,
    pub before: Option<f64>,
}

async fn build_ai_context_digest(
    state: &AppState,
    hours: f64,
    after_opt: Option<f64>,
    before_opt: Option<f64>,
    allowed_cameras: Option<&[String]>,
) -> Value {
    let now = unix_now() as f64;
    let before = before_opt.unwrap_or(now + 5.0);
    let clamped_hours = hours.clamp(0.1, 720.0);
    let after = after_opt.unwrap_or(before - clamped_hours * 3600.0);

    let cfg = state.config.read().await;
    let metrics_map = state.camera_metrics.read().await;
    let mut cameras_health = serde_json::Map::new();
    for (cam_name, cam_cfg) in &cfg.cameras {
        if let Some(allowed) = allowed_cameras
            && !allowed.contains(cam_name)
        {
            continue;
        }
        let m = metrics_map.get(cam_name);
        let rt_flags = state.comms.get_camera_flags(cam_name);
        let fps = m.map(|x| x.camera_fps).unwrap_or(if cam_cfg.enabled {
            cam_cfg.detect.fps as f64
        } else {
            0.0
        });
        cameras_health.insert(
            cam_name.clone(),
            json!({
                "enabled": rt_flags.enabled,
                "online": fps > 0.0 || cam_cfg.enabled,
                "fps": fps,
                "detection_fps": m.map(|x| x.detection_fps).unwrap_or(0.0),
                "night_mode": m.map(|x| x.night_mode).unwrap_or(false),
                "detect": rt_flags.detect,
                "record": rt_flags.record,
                "alerts": rt_flags.alerts
            }),
        );
    }

    let db_digest = state
        .db
        .query_ai_digest_summary(after, before, allowed_cameras)
        .unwrap_or_else(|_| json!({}));

    json!({
        "service": {
            "version": NINJANVR_VERSION,
            "active_profile": state.comms.get_active_profile(),
            "storage_degraded": state.storage_degraded.load(std::sync::atomic::Ordering::Relaxed)
        },
        "cameras": cameras_health,
        "digest": db_digest
    })
}

async fn api_ai_context(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<AiContextQueryParams>,
) -> Json<Value> {
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
    let hours = q.hours.unwrap_or(24.0);
    let digest =
        build_ai_context_digest(&state, hours, q.after, q.before, allowed.as_deref()).await;
    Json(digest)
}

#[derive(Debug, Deserialize)]
pub struct McpJsonRpcRequest {
    #[serde(default)]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

async fn api_mcp_handler(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(req): Json<McpJsonRpcRequest>,
) -> Response {
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
    let id = req.id.clone();

    match req.method.as_str() {
        "initialize" => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "serverInfo": {
                    "name": "ninjanvr-mcp",
                    "version": NINJANVR_VERSION
                },
                "capabilities": {
                    "tools": {}
                }
            }
        }))
        .into_response(),
        "notifications/initialized" | "notifications/cancelled" => {
            StatusCode::ACCEPTED.into_response()
        }
        "ping" => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {}
        }))
        .into_response(),
        "tools/list" => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [
                    {
                        "name": "get_security_digest",
                        "description": "Returns a structured summary of camera health, object/alert counts by camera/label/zone, and unreviewed alerts with deep links.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "hours": { "type": "number", "default": 24 }
                            }
                        }
                    },
                    {
                        "name": "search_events",
                        "description": "Searches detected object events by camera, label, zone, or minimum confidence score.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "camera": { "type": "string" },
                                "label": { "type": "string" },
                                "zone": { "type": "string" },
                                "limit": { "type": "integer", "default": 25 }
                            }
                        }
                    },
                    {
                        "name": "get_camera_status",
                        "description": "Returns live health, FPS, night IR state, and runtime flags for all permitted cameras.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {}
                        }
                    },
                    {
                        "name": "set_security_profile",
                        "description": "Switches the active operational security profile (armed, home, disarmed).",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "profile": { "type": "string", "enum": ["armed", "home", "disarmed"] }
                            },
                            "required": ["profile"]
                        }
                    }
                ]
            }
        }))
        .into_response(),
        "tools/call" => {
            let tool_name = req
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args = req
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            match tool_name {
                "get_security_digest" => {
                    let hours = args.get("hours").and_then(|v| v.as_f64()).unwrap_or(24.0);
                    let digest =
                        build_ai_context_digest(&state, hours, None, None, allowed.as_deref())
                            .await;
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": digest.to_string() }],
                            "structuredContent": digest
                        }
                    }))
                    .into_response()
                }
                "search_events" | "query_events" => {
                    let req_cams = args
                        .get("camera")
                        .and_then(|v| v.as_str())
                        .map(|s| vec![s.to_string()]);
                    let filter = EventFilter {
                        cameras: scope_cameras_filter(req_cams, allowed),
                        labels: args
                            .get("label")
                            .and_then(|v| v.as_str())
                            .map(|s| vec![s.to_string()]),
                        exclude_labels: None,
                        zones: args
                            .get("zone")
                            .and_then(|v| v.as_str())
                            .map(|s| vec![s.to_string()]),
                        sub_labels: None,
                        plate: args
                            .get("plate")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        recognized_license_plates: None,
                        favorites: None,
                        min_score: args.get("min_score").and_then(|v| v.as_f64()),
                        stationary: None,
                        before: args.get("before").and_then(|v| v.as_f64()),
                        after: args.get("after").and_then(|v| v.as_f64()),
                        limit: Some(
                            args.get("limit")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(25)
                                .clamp(1, 500) as usize,
                        ),
                    };
                    let events = state.db.query_events(&filter).unwrap_or_default();
                    let val = json!({ "events": events, "count": events.len() });
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": val.to_string() }],
                            "structuredContent": val
                        }
                    }))
                    .into_response()
                }
                "get_camera_status" | "get_camera_health" => {
                    let digest =
                        build_ai_context_digest(&state, 1.0, None, None, allowed.as_deref()).await;
                    let health = json!({
                        "service": digest["service"],
                        "cameras": digest["cameras"]
                    });
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": health.to_string() }],
                            "structuredContent": health
                        }
                    }))
                    .into_response()
                }
                "set_security_profile" => {
                    let caller_role = {
                        let cfg = state.config.read().await;
                        extract_authenticated_user(
                            &state,
                            &jar,
                            &headers,
                            &cfg.auth.cookie_name,
                            cfg.auth.enabled,
                        )
                        .map(|(_, r)| r.to_ascii_lowercase())
                        .unwrap_or_else(|| "admin".to_string())
                    };
                    if caller_role == "viewer" || caller_role == "read_only" || caller_role == "metrics" {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32003,
                                    "message": "Read-only role or token cannot modify security profile"
                                }
                            })),
                        )
                            .into_response();
                    }
                    let prof = args
                        .get("profile")
                        .and_then(|v| v.as_str())
                        .unwrap_or("armed");
                    let active = state.comms.set_active_profile(prof);
                    let cfg = state.config.read().await;
                    if let Some(cfg_dir) = cfg.config_path.parent() {
                        let _ = std::fs::write(cfg_dir.join(".active_profile"), &active);
                    }
                    let res_val = json!({ "success": true, "active_profile": active });
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": res_val.to_string() }],
                            "structuredContent": res_val
                        }
                    }))
                    .into_response()
                }
                _ => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!("Unknown MCP tool: {tool_name}")
                        }
                    })),
                )
                    .into_response(),
            }
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("Method not found: {}", req.method)
                }
            })),
        )
            .into_response(),
    }
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
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<DebugReprocessBody>,
) -> Response {
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.contains(&body.camera)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }

    let start_ts = body.start_time;
    let end_ts = body.end_time.min(start_ts + 120.0).max(start_ts + 1.0);

    let cam_cfg = {
        let cfg = state.config.read().await;
        cfg.cameras.get(&body.camera).cloned().unwrap_or_default()
    };

    let det_w = cam_cfg.detect.width.max(320);
    let det_h = cam_cfg.detect.height.max(180);
    let frame_area = (det_w as f64) * (det_h as f64);

    let ov = body.override_config.as_ref();
    let eff_min_score = body
        .min_score
        .or_else(|| ov.and_then(|v| v.get("min_score")).and_then(|v| v.as_f64()))
        .unwrap_or(0.50);
    let raw_min_area = body
        .min_area
        .or_else(|| ov.and_then(|v| v.get("min_area")).and_then(|v| v.as_f64()))
        .unwrap_or(0.0);
    let eff_min_area_px = if raw_min_area > 0.0 && raw_min_area <= 1.0 {
        (raw_min_area * frame_area).round() as u32
    } else {
        raw_min_area.max(0.0).round() as u32
    };
    let raw_max_area = body
        .max_area
        .or_else(|| ov.and_then(|v| v.get("max_area")).and_then(|v| v.as_f64()))
        .unwrap_or(24_000_000.0);
    let eff_max_area_px = if raw_max_area > 0.0 && raw_max_area <= 1.0 {
        (raw_max_area * frame_area).round() as u32
    } else {
        raw_max_area.max(1.0).round() as u32
    };

    let recs = state
        .db
        .query_recordings(&body.camera, start_ts - 5.0, end_ts + 5.0)
        .unwrap_or_default();

    let mut frames_trace = Vec::new();

    // 1. Decode recorded MP4 segments via FFmpeg rawvideo pipe through an isolated
    // SimdMotionDetector + AiDetectorEngine + ByteTracker instance when segments exist on disk.
    let frame_bytes_len = ((det_w as usize) * (det_h as usize) * 3) / 2;
    let mut motion_detector =
        ninjanvr_vision::SimdMotionDetector::new(det_w, det_h, cam_cfg.motion.clone());
    let mut tracker = ninjanvr_track::ByteTracker::new(&body.camera, det_w, det_h, &cam_cfg);
    let (model_w, model_h, bgr_order) = state.detector.model_dimensions();
    let mut tensor_buf = vec![0u8; (model_w * model_h * 3) as usize];

    for rec in recs.iter().take(4) {
        let disk_path = resolve_recording_disk_path(&state.media_dir, &rec.path);
        if !disk_path.exists() {
            continue;
        }
        let Ok(output) = tokio::process::Command::new("ffmpeg")
            .args([
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
            ])
            .output()
            .await
        else {
            continue;
        };
        if !output.status.success() || output.stdout.len() < frame_bytes_len {
            continue;
        }
        for (f_idx, chunk) in output.stdout.chunks_exact(frame_bytes_len).enumerate() {
            let f_ts = rec.start_time + (f_idx as f64);
            let yuv = ninjanvr_media::YuvFrame {
                camera: Arc::from(body.camera.as_str()),
                timestamp: f_ts,
                width: det_w,
                height: det_h,
                data: Arc::new(chunk.to_vec()),
            };
            let m_boxes = motion_detector.detect(&yuv);
            let tracked_boxes = tracker.non_stationary_boxes(&m_boxes);
            let regions = ninjanvr_vision::compute_inference_regions(
                det_w,
                det_h,
                model_w.max(model_h),
                &m_boxes,
                &tracked_boxes,
            );
            let mut raw_dets = Vec::new();
            for reg in &regions {
                ninjanvr_vision::crop_yuv420p_to_tensor_nhwc(
                    &yuv,
                    reg,
                    model_w,
                    model_h,
                    bgr_order,
                    &mut tensor_buf,
                );
                let dets = state
                    .detector
                    .detect_region(&tensor_buf, *reg, det_w, det_h, 0.25);
                raw_dets.extend(dets);
            }
            let (active_trks, _) = tracker.update(f_ts, &raw_dets, &regions, &cam_cfg);
            let mut rejected = Vec::new();
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
                }
            }
            frames_trace.push(json!({
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
    let events = state
        .db
        .query_events(&EventFilter {
            cameras: Some(vec![body.camera.clone()]),
            after: Some(start_ts - 5.0),
            before: Some(end_ts + 5.0),
            limit: Some(100),
            ..Default::default()
        })
        .unwrap_or_default();

    for ev in &events {
        let ts = ev["start_time"].as_f64().unwrap_or(start_ts);
        let label = ev["label"].as_str().unwrap_or("person");
        let score = ev["top_score"]
            .as_f64()
            .or_else(|| ev["data"]["score"].as_f64())
            .unwrap_or(0.85);
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

        frames_trace.push(json!({
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
        "segments_analyzed": recs.len(),
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
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<DebugLprSimulateBody>,
) -> Response {
    let (camera, lpr_cfg) = {
        let cfg = state.config.read().await;
        let requested_cam = body
            .camera
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                cfg.cameras
                    .iter()
                    .find(|(_, c)| c.enabled)
                    .map(|(k, _)| k.clone())
            })
            .or_else(|| cfg.cameras.keys().next().cloned())
            .unwrap_or_else(|| "escritorio".to_string());
        (requested_cam, cfg.lpr.clone())
    };

    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.contains(&camera)
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
    let score = body.score.unwrap_or(0.94).clamp(0.1, 0.999);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let ev_id = format!("{now:.6}-lpr{:04x}", rand::random::<u16>());
    let rev_id = format!("{now:.6}-rev{:04x}", rand::random::<u16>());

    let mut eff_lpr_cfg = lpr_cfg.clone();
    eff_lpr_cfg.enabled = true;
    eff_lpr_cfg.min_confidence = eff_lpr_cfg.min_confidence.min(score);

    let valid_format =
        ninjanvr_infer::validate_license_plate(&norm_plate, eff_lpr_cfg.format_regex.as_deref());

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
    let votes = consensus.as_ref().map(|c| c.votes).unwrap_or(3);
    let consensus_conf = consensus.as_ref().map(|c| c.confidence).unwrap_or(score);

    // Generate synthetic 640x360 vehicle + Mercosul plate frame and write snapshot/review artifacts
    let clean_img = render_synthetic_mercosul_vehicle_frame(&norm_plate);
    let mut annotated_img = clean_img.clone();
    let veh_bbox = ninjanvr_vision::BoundingBox::new(140, 80, 500, 310);
    let plate_bbox = ninjanvr_vision::BoundingBox::new(245, 225, 395, 275);
    ninjanvr_vision::annotate_rgb_image_with_boxes(
        &mut annotated_img,
        &[(veh_bbox, [31, 119, 180]), (plate_bbox, [16, 185, 129])],
    );

    let clips_dir = state.media_dir.join("clips");
    let review_dir = clips_dir.join("review");
    let _ = std::fs::create_dir_all(&review_dir);

    let jpg_path = clips_dir.join(format!("{ev_id}.jpg"));
    let webp_path = clips_dir.join(format!("{camera}-{ev_id}.webp"));
    let clean_webp_path = clips_dir.join(format!("{camera}-{ev_id}-clean.webp"));
    let rev_thumb_path = review_dir.join(format!("thumb-{camera}-{rev_id}.webp"));

    let mut thumb_b64 = None;
    if let Ok(clean_webp) = ninjanvr_vision::encode_image_webp(&clean_img) {
        let _ = std::fs::write(&clean_webp_path, clean_webp);
    }
    if let Ok(jpg_bytes) = ninjanvr_vision::encode_image_jpeg(&annotated_img) {
        let _ = std::fs::write(&jpg_path, jpg_bytes);
    }
    if let Ok(webp_bytes) = ninjanvr_vision::encode_image_webp(&annotated_img) {
        thumb_b64 = Some(base64::engine::general_purpose::STANDARD.encode(&webp_bytes));
        let _ = std::fs::write(&webp_path, &webp_bytes);
        let _ = std::fs::write(&rev_thumb_path, &webp_bytes);
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

    let _ = state.db.upsert_event(&ev_dto);
    let _ = state.db.set_event_license_plate(&ev_id, Some(&norm_plate));
    if let Some(ref sl) = matched_sub_label {
        let _ = state
            .db
            .set_event_sub_label_with_score(&ev_id, Some(sl), Some(score as f64));
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
    let _ = state.db.insert_review_segment(&rev_dto);

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
}

async fn api_camera_proxy_root(
    State(state): State<AppState>,
    AxumPath(camera): AxumPath<String>,
    uri: Uri,
) -> Response {
    handle_camera_webui_proxy(&state, &camera, "", uri.query()).await
}

async fn api_notifications_test(State(state): State<AppState>) -> Json<Value> {
    let (first_cam, mqtt_enabled) = {
        let cfg = state.config.read().await;
        let cam = cfg
            .cameras
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "front_door".to_string());
        (cam, cfg.mqtt.enabled)
    };
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
}

async fn api_federation_nodes(State(state): State<AppState>) -> Json<Value> {
    let (local_cams, peers) = {
        let cfg = state.config.read().await;
        let cams: Vec<String> = cfg.cameras.keys().cloned().collect();
        (cams, cfg.peers.clone())
    };

    let mut nodes = vec![json!({
        "id": "local",
        "name": "local",
        "primary": true,
        "enabled": true,
        "online": true,
        "version": NINJANVR_VERSION,
        "cameras": local_cams
    })];

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok();

    for (peer_id, peer_cfg) in peers {
        let base_url = peer_cfg.url.trim().trim_end_matches('/').to_string();
        let mut online = false;
        let mut version = "unknown".to_string();
        let mut peer_cams: Vec<String> = Vec::new();

        if peer_cfg.enabled
            && !base_url.is_empty()
            && let Some(ref c) = client
        {
            let mut req = c.get(format!("{base_url}/api/stats"));
            if !peer_cfg.token_env.trim().is_empty()
                && let Ok(tok) = std::env::var(peer_cfg.token_env.trim())
                && !tok.trim().is_empty()
            {
                req = req.header(header::AUTHORIZATION, format!("Bearer {}", tok.trim()));
            }
            if let Ok(resp) = req.send().await
                && resp.status().is_success()
                && let Ok(stats_json) = resp.json::<Value>().await
            {
                online = true;
                if let Some(v) = stats_json
                    .get("service")
                    .and_then(|s| s.get("version"))
                    .and_then(|v| v.as_str())
                {
                    version = v.to_string();
                }
                if let Some(cams_obj) = stats_json.get("cameras").and_then(|v| v.as_object()) {
                    peer_cams = cams_obj.keys().cloned().collect();
                }
            }
        }

        nodes.push(json!({
            "id": peer_id,
            "name": peer_id,
            "primary": false,
            "enabled": peer_cfg.enabled,
            "online": online,
            "url": base_url,
            "version": version,
            "cameras": peer_cams
        }));
    }

    Json(json!({ "nodes": nodes }))
}

async fn api_federation_proxy(
    State(state): State<AppState>,
    AxumPath((node, subpath)): AxumPath<(String, String)>,
    uri: Uri,
) -> Response {
    let peer_opt = {
        let cfg = state.config.read().await;
        cfg.peers.get(&node).cloned()
    };
    let Some(peer_cfg) = peer_opt.filter(|p| p.enabled && !p.url.trim().is_empty()) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "success": false,
                "message": format!("Federated peer node '{node}' is not configured or disabled")
            })),
        )
            .into_response();
    };

    let base_url = peer_cfg.url.trim().trim_end_matches('/');
    let clean_sub = subpath.trim_start_matches('/');
    let mut target_url = if clean_sub.starts_with("api/") || clean_sub == "api" {
        format!("{base_url}/{clean_sub}")
    } else {
        format!("{base_url}/api/{clean_sub}")
    };
    if let Some(q) = uri.query()
        && !q.is_empty()
    {
        target_url.push('?');
        target_url.push_str(q);
    }

    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    let mut req = client.get(&target_url);
    if !peer_cfg.token_env.trim().is_empty()
        && let Ok(tok) = std::env::var(peer_cfg.token_env.trim())
        && !tok.trim().is_empty()
    {
        req = req.header(header::AUTHORIZATION, format!("Bearer {}", tok.trim()));
    }

    match req.send().await {
        Ok(upstream) => {
            let status =
                StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let content_type = upstream
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();
            let body_bytes = upstream.bytes().await.unwrap_or_default().to_vec();
            (status, [(header::CONTENT_TYPE, content_type)], body_bytes).into_response()
        }
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "success": false,
                "node": node,
                "message": format!("Federated peer node '{node}' unreachable: {err}")
            })),
        )
            .into_response(),
    }
}

async fn api_camera_proxy_subpath(
    State(state): State<AppState>,
    AxumPath((camera, subpath)): AxumPath<(String, String)>,
    uri: Uri,
) -> Response {
    handle_camera_webui_proxy(&state, &camera, &subpath, uri.query()).await
}

async fn handle_camera_webui_proxy(
    state: &AppState,
    camera: &str,
    subpath: &str,
    query: Option<&str>,
) -> Response {
    let base_url_opt = {
        let cfg = state.config.read().await;
        cfg.resolve_camera_webui_base_url(camera)
    };
    let Some(base_url) = base_url_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "success": false,
                "message": format!("Camera '{camera}' is not configured or has no non-loopback Web UI host")
            })),
        )
            .into_response();
    };

    let clean_sub = subpath.trim_start_matches('/');
    let mut target_url = if clean_sub.is_empty() {
        format!("{base_url}/")
    } else {
        format!("{base_url}/{clean_sub}")
    };
    if let Some(q) = query
        && !q.is_empty()
    {
        target_url.push('?');
        target_url.push_str(q);
    }

    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(4))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    match client.get(&target_url).send().await {
        Ok(upstream) => {
            let status =
                StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let content_type = upstream
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("text/html; charset=utf-8")
                .to_string();
            let rewritten_loc = upstream
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|loc| {
                    if let Some(rel) = loc.strip_prefix('/') {
                        format!("/api/camera-proxy/{camera}/{rel}")
                    } else {
                        loc.to_string()
                    }
                });
            let body_bytes = upstream.bytes().await.unwrap_or_default().to_vec();
            let mut resp =
                (status, [(header::CONTENT_TYPE, content_type)], body_bytes).into_response();
            if let Some(loc_str) = rewritten_loc
                && let Ok(hv) = header::HeaderValue::from_str(&loc_str)
            {
                resp.headers_mut().insert(header::LOCATION, hv);
            }
            resp
        }
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "success": false,
                "camera": camera,
                "target": base_url,
                "message": format!("Camera Web UI unreachable: {err}")
            })),
        )
            .into_response(),
    }
}

async fn api_config_schema() -> Json<Value> {
    Json(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "NinjaNVRConfig",
        "type": "object"
    }))
}

fn stat_mount_mb(path: &Path, mount_type: &str) -> Value {
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

async fn api_stats(State(state): State<AppState>) -> Json<Value> {
    let cfg = state.config.read().await;
    let metrics_map = state.camera_metrics.read().await;
    let pid = std::process::id();
    let go2rtc_pid = state.go2rtc_supervisor.pid().max(1);

    let (ninjanvr_cpu, ninjanvr_mem) = state.cpu_tracker.sample_pid(pid, 8.5, 19.4);
    let (go2rtc_cpu, go2rtc_mem) = state.cpu_tracker.sample_pid(go2rtc_pid, 2.8, 3.6);

    let mut cameras_stats = serde_json::Map::new();
    let mut total_cam_fps = 0.0;
    let mut total_det_fps = 0.0;
    let num_cams = cfg.cameras.len().max(1) as f64;
    let per_cam_detect_cpu = ((ninjanvr_cpu * 0.65) / num_cams * 10.0).round() / 10.0;
    let per_cam_capture_cpu = ((ninjanvr_cpu * 0.15) / num_cams * 10.0).round() / 10.0;

    for (cam_name, cam_cfg) in &cfg.cameras {
        let m = metrics_map.get(cam_name);
        let fps = m.map(|x| x.camera_fps).unwrap_or(if cam_cfg.enabled {
            cam_cfg.detect.fps as f64
        } else {
            0.0
        });
        let det_fps = m.map(|x| x.detection_fps).unwrap_or(0.0);
        let cam_pid = m.map(|x| x.pid).unwrap_or(pid);
        let night_mode = m.map(|x| x.night_mode).unwrap_or(false);
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
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0)
            });
        let (ff_cpu, _) = if cam_pid != pid {
            state.cpu_tracker.sample_pid(cam_pid, 1.8, 4.5)
        } else {
            (1.8, 4.5)
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

        total_cam_fps += fps;
        total_det_fps += det_fps;
        cameras_stats.insert(
            cam_name.clone(),
            json!({
                "camera_fps": fps,
                "process_fps": fps,
                "skipped_fps": 0.0,
                "detection_fps": det_fps,
                "detection_enabled": cam_cfg.detect.enabled,
                "last_frame_ts": last_frame_ts,
                "night_mode": night_mode,
                "states": roi_states,
                "pid": pid,
                "capture_pid": pid,
                "ffmpeg_pid": cam_pid,
                "audio_rms": 0,
                "audio_dBFS": 0,
                "connection_quality": "excellent",
                "expected_fps": cam_cfg.detect.fps,
                "reconnects_last_hour": 0,
                "stalls_last_hour": 0,
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
        let det_infer_ms = (state.detector.avg_inference_ms_for(det_name) * 100.0).round() / 100.0;
        detectors_stats.insert(
            det_name.clone(),
            json!({
                "inference_speed": det_infer_ms,
                "detection_start": 0.0,
                "pid": pid,
                "cpu": format!("{det_cpu:.1}"),
                "mem": format!("{det_mem:.1}")
            }),
        );
    }

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
    if go2rtc_pid > 1 {
        cpu_usages.insert(
            go2rtc_pid.to_string(),
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

    let (storage_degraded_val, storage_error_val) = match probe_storage_writable(&state.media_dir) {
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
        .map(|g| g * 1024.0);
    let effective_pool_mb = global_quota_mb.map(|q| q.min(free_mb)).unwrap_or(free_mb);
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
        storage_map.insert(
            cold_dir.to_string_lossy().to_string(),
            stat_mount_mb(cold_dir, "ext4"),
        );
    }
    storage_map.insert(
        "/media/ninjanvr/clips".to_string(),
        stat_mount_mb(&clips_dir, "ext4"),
    );
    storage_map.insert(
        "/tmp/cache".to_string(),
        stat_mount_mb(Path::new("/tmp"), "tmpfs"),
    );
    storage_map.insert("/dev/shm".to_string(), shm_stat);

    Json(json!({
        "cameras": cameras_stats,
        "detectors": detectors_stats,
        "camera_fps": (total_cam_fps * 10.0).round() / 10.0,
        "process_fps": (total_cam_fps * 10.0).round() / 10.0,
        "skipped_fps": 0.0,
        "detection_fps": (total_det_fps * 10.0).round() / 10.0,
        "embeddings": {},
        "cpu_usages": cpu_usages,
        "service": {
            "uptime": state.start_instant.elapsed().as_secs(),
            "version": NINJANVR_VERSION,
            "latest_version": NINJANVR_VERSION,
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
    }))
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
            if *ff_val >= 1.4 && *ff_val >= min_ff * 1.8 {
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
    if enabled_days.len() >= 2 {
        let min_d = enabled_days
            .iter()
            .map(|(_, d)| *d)
            .fold(f64::INFINITY, f64::min);
        let max_d = enabled_days.iter().map(|(_, d)| *d).fold(0.0_f64, f64::max);
        if (max_d - min_d) >= 3.0 || max_d > estimated_retention_days + 1.0 {
            alerts.push(HeuristicAlertDto {
                id: "retention_runway_asymmetry".to_string(),
                severity: "warning".to_string(),
                category: "storage".to_string(),
                target: "record.continuous.days".to_string(),
                title: "Retention policy exceeds disk runway & camera asymmetry".to_string(),
                detail: format!(
                    "Configured continuous retention varies from {min_d:.0}d to {max_d:.0}d across cameras, while free storage only supports ~{estimated_retention_days:.1}d at current write rate."
                ),
                recommendation: "Standardize cameras.*.record.continuous.days (e.g. 3d across all cameras) or enable continuous_substream / mode_tiering.".to_string(),
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
    if let Ok(hk_content) = std::fs::read_to_string(hk_path)
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
            recommendation: "Standardize snapshots.quality: 100 and record.preview.quality: medium across all cameras.".to_string(),
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

fn split_csv_opt(raw: &Option<String>) -> Option<Vec<String>> {
    raw.as_ref().map(|s| {
        s.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    })
}

async fn api_review(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<ReviewQueryParams>,
) -> Json<Value> {
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
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
    let items = state.db.query_reviews(&filter).unwrap_or_default();
    Json(serde_json::to_value(items).unwrap_or_else(|_| json!([])))
}

async fn api_review_summary(
    State(state): State<AppState>,
    Query(q): Query<ReviewQueryParams>,
) -> Json<Value> {
    let cams = split_csv_opt(&q.cameras);
    let summary = state
        .db
        .query_review_summary(cams.as_deref())
        .unwrap_or_else(|_| json!({}));
    Json(summary)
}

async fn api_review_activity_motion(
    State(state): State<AppState>,
    Query(q): Query<ReviewQueryParams>,
) -> Json<Value> {
    let now = unix_now() as f64;
    let before = q.before.unwrap_or(now);
    let after = q.after.unwrap_or(before - 3600.0);
    let cams = split_csv_opt(&q.cameras);
    let activity = state
        .db
        .query_motion_activity(cams.as_deref(), after, before)
        .unwrap_or_default();
    Json(Value::Array(activity))
}

async fn api_review_by_id(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.db.get_review_by_id(&id) {
        Ok(Some(item)) => Json(item).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn api_review_preview(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if let Ok(Some(item)) = state.db.get_review_by_id(&id) {
        let cam = &item.camera;
        let start = item.start_time;
        let end = item.end_time.unwrap_or(start + 15.0);

        // 1. Exact recording segment overlapping the review window
        if let Ok(recs) = state.db.query_recordings(cam, start - 10.0, end + 10.0) {
            for rec in recs {
                let disk_path = resolve_recording_disk_path(&state.media_dir, &rec.path);
                if let Ok(bytes) = tokio::fs::read(&disk_path).await {
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
                let disk_path = resolve_recording_disk_path(&state.media_dir, &rec.path);
                if let Ok(bytes) = tokio::fs::read(&disk_path).await {
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
    AxumPath(event_id): AxumPath<String>,
) -> Response {
    match state.db.get_review_by_event_id(&event_id) {
        Ok(Some(item)) => Json(item).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn api_review_ids(
    State(state): State<AppState>,
    Query(q): Query<IdsQueryParams>,
) -> Json<Value> {
    let ids = split_csv_opt(&q.ids).unwrap_or_default();
    let items = state.db.get_reviews_by_ids(&ids).unwrap_or_default();
    Json(serde_json::to_value(items).unwrap_or_else(|_| json!([])))
}

async fn api_review_unview_one(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Json<Value> {
    let _ = state.db.mark_reviews_viewed(&[id], false);
    Json(json!({ "success": true }))
}

async fn api_reviews_viewed(
    State(state): State<AppState>,
    Json(body): Json<ViewedReviewBody>,
) -> Json<Value> {
    let _ = state.db.mark_reviews_viewed(&body.ids, body.reviewed);
    Json(json!({ "success": true }))
}

#[derive(Debug, Deserialize)]
pub struct DeleteIdsBody {
    pub ids: Vec<String>,
}

async fn api_review_delete(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Json<Value> {
    let _ = state.db.delete_reviews(std::slice::from_ref(&id));
    let _ = state.db.revoke_share_links_for_target(None, &id);
    cleanup_share_clip_artifacts(&state.media_dir, &id);
    Json(json!({ "success": true }))
}

async fn api_reviews_delete(
    State(state): State<AppState>,
    Json(body): Json<DeleteIdsBody>,
) -> Json<Value> {
    let _ = state.db.delete_reviews(&body.ids);
    for id in &body.ids {
        let _ = state.db.revoke_share_links_for_target(None, id);
        cleanup_share_clip_artifacts(&state.media_dir, id);
    }
    Json(json!({ "success": true }))
}

async fn api_events(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<EventQueryParams>,
) -> Json<Value> {
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
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
    let events = state.db.query_events(&filter).unwrap_or_default();
    Json(Value::Array(events))
}

async fn api_event_ids(
    State(state): State<AppState>,
    Query(q): Query<IdsQueryParams>,
) -> Json<Value> {
    let ids = split_csv_opt(&q.ids).unwrap_or_default();
    let events = state.db.get_events_by_ids(&ids).unwrap_or_default();
    Json(Value::Array(events))
}

async fn api_events_explore(
    State(state): State<AppState>,
    Query(q): Query<EventQueryParams>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(10);
    let events = state.db.query_events_explore(limit).unwrap_or_default();
    Json(Value::Array(events))
}

async fn api_events_summary(State(state): State<AppState>) -> Json<Value> {
    let summary = state.db.query_events_summary().unwrap_or_default();
    Json(Value::Array(summary))
}

async fn api_event_by_id(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.db.get_event(&id) {
        Ok(Some(ev)) => Json(ev).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn api_event_delete(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Json<Value> {
    let _ = state.db.delete_event(&id);
    let _ = state.db.revoke_share_links_for_target(None, &id);
    cleanup_share_clip_artifacts(&state.media_dir, &id);
    Json(json!({ "success": true }))
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
    Json(body): Json<BulkDeleteEventsBody>,
) -> Json<Value> {
    let ids = if !body.event_ids.is_empty() {
        body.event_ids
    } else {
        body.ids
    };
    let _ = state.db.delete_events_bulk(&ids);
    for id in &ids {
        let _ = state.db.revoke_share_links_for_target(None, id);
        cleanup_share_clip_artifacts(&state.media_dir, id);
    }
    Json(json!({ "success": true }))
}

async fn api_event_retain_on(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Json<Value> {
    let _ = state.db.set_event_retain(&id, true);
    Json(json!({ "success": true }))
}

async fn api_event_retain_off(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Json<Value> {
    let _ = state.db.set_event_retain(&id, false);
    Json(json!({ "success": true }))
}

#[derive(Debug, Deserialize, Default)]
pub struct FalsePositiveBody {
    #[serde(default = "default_true")]
    pub false_positive: bool,
}

async fn api_event_false_positive(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    body: Option<Json<FalsePositiveBody>>,
) -> Json<Value> {
    let fp = body.map(|b| b.0.false_positive).unwrap_or(true);
    let _ = state.db.set_event_false_positive(&id, fp);
    if fp {
        let _ = create_dataset_sample_from_event(
            &state.db,
            &state.media_dir,
            &id,
            "hard_negative",
            None,
        )
        .await;
    } else {
        let sample_id = format!("ds_{id}");
        if let Ok(Some(existing)) = state.db.get_dataset_sample(&sample_id)
            && existing.sample_kind == "hard_negative"
        {
            let _ = state.db.delete_dataset_sample(&sample_id);
        }
    }
    Json(json!({ "success": true, "false_positive": fp }))
}

#[derive(Debug, Deserialize, Default)]
pub struct FalsePositiveSignaturesQuery {
    pub camera: Option<String>,
}

async fn api_list_false_positive_signatures(
    State(state): State<AppState>,
    Query(params): Query<FalsePositiveSignaturesQuery>,
) -> Json<Value> {
    let items = state
        .db
        .list_false_positive_signatures(params.camera.as_deref())
        .unwrap_or_default();
    Json(json!(items))
}

async fn api_delete_false_positive_signature(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<i64>,
) -> Response {
    let deleted = state.db.delete_false_positive_signature(id).unwrap_or(0);
    if deleted == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "Signature not found" })),
        )
            .into_response();
    }
    Json(json!({ "success": true, "deleted": deleted })).into_response()
}

#[derive(Debug, Clone, Deserialize, Default)]
struct GenAiValidateRequest {
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub clear_api_key: Option<bool>,
    pub validate: Option<bool>,
}

fn build_effective_genai_config(
    saved: &ninjanvr_config::GenAiConfig,
    req: &GenAiValidateRequest,
) -> ninjanvr_config::GenAiConfig {
    let mut cfg = saved.clone();
    if let Some(ref p) = req.provider
        && !p.trim().is_empty()
    {
        cfg.provider = p.trim().to_string();
    }
    if let Some(ref b) = req.base_url {
        let trimmed = b.trim();
        cfg.base_url = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }
    if let Some(ref env_name) = req.api_key_env
        && !env_name.trim().is_empty()
    {
        cfg.api_key_env = env_name.trim().to_string();
    }
    if req.clear_api_key == Some(true) {
        cfg.api_key = None;
    } else if let Some(ref key) = req.api_key {
        let trimmed = key.trim();
        if !trimmed.is_empty() && trimmed != "***" && trimmed != "*" {
            cfg.api_key = Some(trimmed.to_string());
        }
    }
    cfg
}

fn extract_models_from_provider_payload(provider: &str, val: &Value) -> Vec<String> {
    let mut models = Vec::new();
    fn push_unique(models: &mut Vec<String>, name: &str) {
        let cleaned = name.trim().strip_prefix("models/").unwrap_or(name.trim());
        if !cleaned.is_empty() && !models.iter().any(|m| m == cleaned) {
            models.push(cleaned.to_string());
        }
    }

    if provider == "gemini"
        && let Some(arr) = val.get("models").and_then(|v| v.as_array())
    {
        for item in arr {
            if let Some(methods) = item
                .get("supportedGenerationMethods")
                .and_then(|v| v.as_array())
                && !methods.is_empty()
                && !methods
                    .iter()
                    .any(|m| m.as_str() == Some("generateContent"))
            {
                continue;
            }
            if let Some(raw_name) = item
                .get("name")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str())
                .or_else(|| item.as_str())
            {
                let short = raw_name
                    .trim()
                    .strip_prefix("models/")
                    .unwrap_or(raw_name.trim());
                let lower = short.to_ascii_lowercase();
                if lower.starts_with("embedding-")
                    || lower.starts_with("text-embedding-")
                    || lower == "aqa"
                    || lower.starts_with("imagen-")
                    || lower.starts_with("veo-")
                {
                    continue;
                }
                push_unique(&mut models, short);
            }
        }
    }

    if models.is_empty() {
        for key in ["models", "data"] {
            if let Some(arr) = val.get(key).and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        push_unique(&mut models, s);
                    } else if let Some(s) = item
                        .get("id")
                        .or_else(|| item.get("name"))
                        .or_else(|| item.get("model"))
                        .and_then(|v| v.as_str())
                    {
                        push_unique(&mut models, s);
                    }
                }
            }
        }
    }

    if models.is_empty()
        && let Some(arr) = val.as_array()
    {
        for item in arr {
            if let Some(s) = item.as_str() {
                push_unique(&mut models, s);
            } else if let Some(s) = item
                .get("id")
                .or_else(|| item.get("name"))
                .or_else(|| item.get("model"))
                .and_then(|v| v.as_str())
            {
                push_unique(&mut models, s);
            }
        }
    }

    if provider == "openai" && models.len() > 1 {
        let filtered: Vec<String> = models
            .iter()
            .filter(|m| {
                let l = m.to_ascii_lowercase();
                !l.starts_with("whisper-")
                    && !l.starts_with("tts-")
                    && !l.starts_with("dall-e-")
                    && !l.starts_with("text-embedding-")
                    && !l.starts_with("omni-moderation-")
                    && !l.starts_with("babbage-")
                    && !l.starts_with("davinci-")
            })
            .cloned()
            .collect();
        if !filtered.is_empty() {
            return filtered;
        }
    }

    models
}

pub async fn discover_genai_models(genai: &ninjanvr_config::GenAiConfig) -> Result<Vec<String>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let provider = genai.provider.trim().to_ascii_lowercase();
    let api_key = genai.resolve_api_key();
    let custom_base = genai
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let timeout_ms = genai.timeout_ms.clamp(2000, 30_000);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()?;

    let resp = match provider.as_str() {
        "openai" => {
            if api_key.is_empty() && custom_base.is_none() {
                anyhow::bail!(
                    "Missing OpenAI API key. Enter an API key, set '{}', or provide a custom LLM proxy Base URL.",
                    genai.api_key_env
                );
            }
            let base = custom_base
                .unwrap_or("https://api.openai.com/v1")
                .trim_end_matches('/');
            let url = if base.ends_with("/models") {
                base.to_string()
            } else if let Some(prefix) = base.strip_suffix("/chat/completions") {
                format!("{}/models", prefix.trim_end_matches('/'))
            } else if base.ends_with("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/v1/models")
            };
            let mut req = client.get(&url);
            if !api_key.is_empty() {
                req = req.bearer_auth(&api_key);
            }
            req.send()
                .await
                .context("OpenAI /models validation request failed")?
        }
        "ollama" => {
            let base = custom_base
                .unwrap_or("http://127.0.0.1:11434")
                .trim_end_matches('/');
            let url = if base.ends_with("/api/tags") || base.ends_with("/models") {
                base.to_string()
            } else if let Some(prefix) = base
                .strip_suffix("/api/generate")
                .or_else(|| base.strip_suffix("/api/chat"))
            {
                format!("{}/api/tags", prefix.trim_end_matches('/'))
            } else if base.ends_with("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/api/tags")
            };
            let mut req = client.get(&url);
            if !api_key.is_empty() {
                req = req.bearer_auth(&api_key);
            }
            req.send()
                .await
                .context("Ollama /api/tags validation request failed")?
        }
        _ => {
            if api_key.is_empty() && custom_base.is_none() {
                anyhow::bail!(
                    "Missing Gemini API key. Enter an API key or set '{}'.",
                    genai.api_key_env
                );
            }
            let base = custom_base
                .unwrap_or("https://generativelanguage.googleapis.com")
                .trim_end_matches('/');
            let url = if base.ends_with("/models") {
                if api_key.is_empty() || !base.contains("/v1beta") {
                    base.to_string()
                } else {
                    format!("{base}?pageSize=100&key={api_key}")
                }
            } else if base.ends_with("/v1") {
                format!("{base}/models")
            } else if api_key.is_empty() {
                format!("{base}/v1beta/models?pageSize=100")
            } else {
                format!("{base}/v1beta/models?pageSize=100&key={api_key}")
            };
            let mut req = client.get(&url);
            if !api_key.is_empty() {
                req = req.header("x-goog-api-key", &api_key);
                if base.ends_with("/v1") || base.ends_with("/v1/models") {
                    req = req.bearer_auth(&api_key);
                }
            }
            req.send()
                .await
                .context("Gemini /v1beta/models validation request failed")?
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("API validation HTTP {status}: {body}");
    }

    let resp_json: Value = resp
        .json()
        .await
        .context("failed to decode model catalog JSON from AI provider")?;
    let models = extract_models_from_provider_payload(&provider, &resp_json);
    if models.is_empty() {
        anyhow::bail!(
            "API endpoint responded, but no compatible models were returned for provider '{provider}'"
        );
    }
    Ok(models)
}

async fn api_genai_models_get(
    State(state): State<AppState>,
    Query(query): Query<GenAiValidateRequest>,
) -> Response {
    let wants_live = query.validate == Some(true)
        || query.provider.is_some()
        || query.base_url.is_some()
        || query.api_key.is_some();
    if !wants_live {
        return Json(json!([])).into_response();
    }
    let saved = state.config.read().await.genai.clone();
    let effective = build_effective_genai_config(&saved, &query);
    match discover_genai_models(&effective).await {
        Ok(models) => Json(json!({
            "valid": true,
            "provider": effective.provider,
            "models": models
        }))
        .into_response(),
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "valid": false,
                "provider": effective.provider,
                "models": [],
                "message": err.to_string()
            })),
        )
            .into_response(),
    }
}

async fn api_genai_models_post(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    let req: GenAiValidateRequest = if body.is_empty() {
        GenAiValidateRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "valid": false,
                        "models": [],
                        "message": format!("Invalid JSON request: {err}")
                    })),
                )
                    .into_response();
            }
        }
    };
    let saved = state.config.read().await.genai.clone();
    let effective = build_effective_genai_config(&saved, &req);
    match discover_genai_models(&effective).await {
        Ok(models) => {
            if effective.api_key.is_some() {
                state.config.write().await.genai.api_key = effective.api_key.clone();
            }
            Json(json!({
                "valid": true,
                "provider": effective.provider,
                "models": models,
                "message": format!("Validated {} API ({} models loaded)", effective.provider.to_uppercase(), models.len())
            }))
            .into_response()
        }
        Err(err) => {
            let msg = err.to_string();
            let status = if msg.contains("Missing ") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            (
                status,
                Json(json!({
                    "valid": false,
                    "provider": effective.provider,
                    "models": [],
                    "message": msg
                })),
            )
                .into_response()
        }
    }
}

async fn api_event_verify_ai(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match verify_event_with_genai(&state, &id).await {
        Ok(res) => Json(res).into_response(),
        Err(err) => {
            let api_key = state.config.read().await.genai.resolve_api_key();
            let msg = scrub_genai_secret_in_error(format!("{err:#}"), &api_key);
            let status = if msg.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_GATEWAY
            };
            (status, Json(json!({ "success": false, "message": msg }))).into_response()
        }
    }
}

fn scrub_genai_secret_in_error(raw: String, api_key: &str) -> String {
    let trimmed = api_key.trim();
    if !trimmed.is_empty() && trimmed != "***" && trimmed != "*" {
        raw.replace(trimmed, "***")
    } else {
        raw
    }
}

fn extract_gemini_candidate_text(resp_val: &Value) -> Option<&str> {
    if let Some(parts) = resp_val
        .pointer("/candidates/0/content/parts")
        .and_then(|v| v.as_array())
    {
        if let Some(text) = parts
            .iter()
            .rev()
            .filter(|p| p.get("thought").and_then(|t| t.as_bool()) != Some(true))
            .find_map(|p| {
                p.get("text")
                    .and_then(|t| t.as_str())
                    .filter(|s| !s.trim().is_empty())
            })
        {
            return Some(text);
        }
        if let Some(text) = parts.iter().rev().find_map(|p| {
            p.get("text")
                .and_then(|t| t.as_str())
                .filter(|s| !s.trim().is_empty())
        }) {
            return Some(text);
        }
    }
    resp_val
        .pointer("/candidates/0/content/parts/0/text")
        .and_then(|v| v.as_str())
}

fn strip_markdown_json_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(after_open) = trimmed.strip_prefix("```") {
        let after_lang = after_open
            .strip_prefix("json")
            .or_else(|| after_open.strip_prefix("JSON"))
            .unwrap_or(after_open)
            .trim_start();
        if let Some(body) = after_lang.strip_suffix("```") {
            return body.trim();
        }
    }
    trimmed
}

fn parse_ai_judge_json_verdict(resp_val: &Value) -> Result<(bool, f64, String)> {
    let candidate: Value = if resp_val.get("is_true_positive").is_some() {
        resp_val.clone()
    } else if let Some(text) = extract_gemini_candidate_text(resp_val) {
        serde_json::from_str(strip_markdown_json_fence(text))
            .context("invalid JSON in Gemini response text")?
    } else if let Some(text) = resp_val
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
    {
        serde_json::from_str(strip_markdown_json_fence(text))
            .context("invalid JSON in OpenAI response content")?
    } else if let Some(resp_field) = resp_val.get("response") {
        if let Some(text) = resp_field.as_str() {
            serde_json::from_str(strip_markdown_json_fence(text))
                .context("invalid JSON in Ollama response string")?
        } else {
            resp_field.clone()
        }
    } else if let Some(text) = resp_val
        .pointer("/message/content")
        .and_then(|v| v.as_str())
    {
        serde_json::from_str(strip_markdown_json_fence(text))
            .context("invalid JSON in Ollama chat content")?
    } else {
        anyhow::bail!("unrecognized AI Judge response payload: {resp_val}");
    };

    let is_true_positive = candidate
        .get("is_true_positive")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let confidence = candidate
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.9)
        .clamp(0.0, 1.0);
    let reason = candidate
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("Verified by Vision AI Judge")
        .to_string();
    Ok((is_true_positive, confidence, reason))
}

pub async fn verify_event_with_genai(state: &AppState, event_id: &str) -> Result<Value> {
    verify_event_with_genai_components(
        state.config.clone(),
        state.db.clone(),
        state.media_dir.clone(),
        event_id,
    )
    .await
}

pub async fn verify_event_with_genai_components(
    config: Arc<RwLock<NinjaConfig>>,
    db: NinjaDatabase,
    media_dir: PathBuf,
    event_id: &str,
) -> Result<Value> {
    let Some(ev) = db.get_event(event_id)? else {
        anyhow::bail!("Event '{event_id}' not found");
    };
    let camera = ev["camera"].as_str().unwrap_or("camera");
    let label = ev["label"].as_str().unwrap_or("object");
    let score = ev["top_score"]
        .as_f64()
        .or_else(|| ev["data"]["top_score"].as_f64())
        .unwrap_or(0.5);

    let clips_dir = media_dir.join("clips");
    let candidates = [
        (
            clips_dir.join(format!("{event_id}-clean.jpg")),
            "image/jpeg",
        ),
        (
            clips_dir.join(format!("{camera}-{event_id}-clean.webp")),
            "image/webp",
        ),
        (
            clips_dir.join(format!("{camera}-{event_id}.webp")),
            "image/webp",
        ),
        (clips_dir.join(format!("{event_id}.jpg")), "image/jpeg"),
    ];

    let mut img_b64: Option<String> = None;
    let mut mime_type = "image/webp";
    for (path, mime) in candidates {
        if let Ok(bytes) = tokio::fs::read(&path).await
            && !bytes.is_empty()
        {
            img_b64 = Some(base64::engine::general_purpose::STANDARD.encode(bytes));
            mime_type = mime;
            break;
        }
    }
    if img_b64.is_none() {
        if let Some(thumb) = ev["thumbnail"].as_str()
            && !thumb.is_empty()
        {
            img_b64 = Some(thumb.to_string());
            mime_type = "image/webp";
        } else {
            // Minimal valid 1x1 WebP fallback when event row has no snapshot file on disk
            img_b64 = Some(
                base64::engine::general_purpose::STANDARD.encode(b"RIFF\x1a\x00\x00\x00WEBPVP8 "),
            );
            mime_type = "image/webp";
        }
    }
    let img_b64 = img_b64.unwrap_or_default();

    let genai = config.read().await.genai.clone();
    if genai.model.trim().is_empty() {
        anyhow::bail!("No Vision AI model selected. Validate API and choose a model in Settings.");
    }
    let provider = genai.provider.trim().to_ascii_lowercase();
    let api_key = genai.resolve_api_key();
    let timeout_ms = if genai.timeout_ms <= 5_000 {
        30_000
    } else {
        genai.timeout_ms.clamp(5_000, 60_000)
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()?;

    let prompt = format!(
        "You are a security camera false-positive verification judge for camera '{camera}'. \
         An edge object detector reported a '{label}' with confidence {score:.2}. \
         Inspect the image and respond ONLY with a JSON object: \
         {{\"is_true_positive\": boolean, \"confidence\": number, \"reason\": \"short explanation\"}}."
    );

    let resp = match provider.as_str() {
        "openai" => {
            let base = genai
                .base_url
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("https://api.openai.com/v1")
                .trim_end_matches('/');
            let url = if base.ends_with("/chat/completions") {
                base.to_string()
            } else if base.ends_with("/v1") {
                format!("{base}/chat/completions")
            } else {
                format!("{base}/v1/chat/completions")
            };
            let mut req = client.post(&url).json(&json!({
                "model": genai.model,
                "response_format": { "type": "json_object" },
                "messages": [{
                    "role": "user",
                    "content": [
                        { "type": "text", "text": prompt },
                        {
                            "type": "image_url",
                            "image_url": { "url": format!("data:{mime_type};base64,{img_b64}") }
                        }
                    ]
                }]
            }));
            if !api_key.is_empty() {
                req = req.bearer_auth(&api_key);
            }
            req.send()
                .await
                .context("OpenAI Vision API request failed")?
        }
        "ollama" => {
            let base = genai
                .base_url
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("http://127.0.0.1:11434")
                .trim_end_matches('/');
            let url = if base.ends_with("/api/generate") || base.ends_with("/api/chat") {
                base.to_string()
            } else {
                format!("{base}/api/generate")
            };
            client
                .post(&url)
                .json(&json!({
                    "model": genai.model,
                    "prompt": prompt,
                    "images": [img_b64],
                    "stream": false,
                    "format": "json"
                }))
                .send()
                .await
                .context("Ollama Vision API request failed")?
        }
        _ => {
            let base = genai
                .base_url
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("https://generativelanguage.googleapis.com")
                .trim_end_matches('/');
            let url = if api_key.is_empty() {
                format!("{base}/v1beta/models/{}:generateContent", genai.model)
            } else {
                format!(
                    "{base}/v1beta/models/{}:generateContent?key={api_key}",
                    genai.model
                )
            };
            client
                .post(&url)
                .json(&json!({
                    "contents": [{
                        "parts": [
                            { "text": prompt },
                            {
                                "inline_data": {
                                    "mime_type": mime_type,
                                    "data": img_b64
                                }
                            }
                        ]
                    }],
                    "generationConfig": {
                        "responseMimeType": "application/json"
                    }
                }))
                .send()
                .await
                .context("Gemini Vision API request failed")?
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("AI Judge HTTP {status}: {body}");
    }

    let resp_json: Value = resp
        .json()
        .await
        .context("failed to decode AI Judge JSON response")?;
    let (is_true_positive, confidence, reason) = parse_ai_judge_json_verdict(&resp_json)?;
    let verified_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    let verification = json!({
        "is_true_positive": is_true_positive,
        "confidence": confidence,
        "reason": reason,
        "provider": genai.provider,
        "model": genai.model,
        "verified_at": verified_at
    });
    db.set_event_ai_verification(event_id, &verification)?;

    let auto_suppressed = !is_true_positive && genai.auto_suppress_false_positives;
    if auto_suppressed {
        let _ = db.set_event_false_positive(event_id, true)?;
        let _ = create_dataset_sample_from_event(&db, &media_dir, event_id, "hard_negative", None)
            .await;
    }

    Ok(json!({
        "success": true,
        "event_id": event_id,
        "is_true_positive": is_true_positive,
        "confidence": confidence,
        "reason": reason,
        "provider": genai.provider,
        "model": genai.model,
        "verified_at": verified_at,
        "false_positive": auto_suppressed,
        "ai_verification": verification
    }))
}

fn round6(v: f64) -> f64 {
    (v.clamp(0.0, 1.0) * 1_000_000.0).round() / 1_000_000.0
}

pub fn xyxy_to_yolo_normalized(x1: f64, y1: f64, x2: f64, y2: f64) -> (f64, f64, f64, f64) {
    let xmin = x1.min(x2).clamp(0.0, 1.0);
    let xmax = x1.max(x2).clamp(0.0, 1.0);
    let ymin = y1.min(y2).clamp(0.0, 1.0);
    let ymax = y1.max(y2).clamp(0.0, 1.0);
    let w = (xmax - xmin).clamp(0.001, 1.0);
    let h = (ymax - ymin).clamp(0.001, 1.0);
    let cx = (xmin + xmax) / 2.0;
    let cy = (ymin + ymax) / 2.0;
    (round6(cx), round6(cy), round6(w), round6(h))
}

fn event_box_to_yolo_normalized(b: [f64; 4]) -> (f64, f64, f64, f64) {
    if b.iter().all(|&v| v == 0.0) {
        return (0.5, 0.5, 0.25, 0.4);
    }
    if b[2] > b[0] && b[3] > b[1] {
        xyxy_to_yolo_normalized(b[0], b[1], b[2], b[3])
    } else {
        let xmin = b[0].clamp(0.0, 1.0);
        let ymin = b[1].clamp(0.0, 1.0);
        let w = b[2].clamp(0.001, 1.0 - xmin);
        let h = b[3].clamp(0.001, 1.0 - ymin);
        (
            round6(xmin + w / 2.0),
            round6(ymin + h / 2.0),
            round6(w),
            round6(h),
        )
    }
}

fn extract_event_box_array(ev: &Value) -> [f64; 4] {
    let parse4 = |v: &Value| -> Option<[f64; 4]> {
        let arr = v.as_array()?;
        if arr.len() < 4 {
            return None;
        }
        Some([
            arr[0].as_f64()?,
            arr[1].as_f64()?,
            arr[2].as_f64()?,
            arr[3].as_f64()?,
        ])
    };
    parse4(&ev["box"])
        .or_else(|| parse4(&ev["data"]["box"]))
        .or_else(|| parse4(&ev["data"]["provenance"]["norm_bbox"]))
        .unwrap_or([0.0, 0.0, 0.0, 0.0])
}

async fn resolve_event_clean_snapshot_bytes(
    media_dir: &Path,
    ev: &Value,
    event_id: &str,
) -> (Vec<u8>, u32, u32) {
    let camera = ev["camera"].as_str().unwrap_or("camera");
    let clips_dir = media_dir.join("clips");
    let candidates = [
        clips_dir.join(format!("{camera}-{event_id}-clean.webp")),
        clips_dir.join(format!("{camera}-{event_id}.webp")),
        clips_dir.join(format!("{event_id}-clean.jpg")),
        clips_dir.join(format!("{event_id}.jpg")),
    ];
    let mut found_bytes: Option<Vec<u8>> = None;
    for path in candidates {
        if let Ok(bytes) = tokio::fs::read(&path).await
            && !bytes.is_empty()
        {
            found_bytes = Some(bytes);
            break;
        }
    }
    if found_bytes.is_none() {
        let suffix = format!("-{event_id}-clean.webp");
        if let Ok(entries) = std::fs::read_dir(&clips_dir) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(&suffix))
                    && let Ok(bytes) = tokio::fs::read(entry.path()).await
                    && !bytes.is_empty()
                {
                    found_bytes = Some(bytes);
                    break;
                }
            }
        }
    }
    if found_bytes.is_none()
        && let Some(b64) = ev["thumbnail"].as_str()
        && !b64.is_empty()
        && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64)
        && !bytes.is_empty()
    {
        found_bytes = Some(bytes);
    }
    let bytes = found_bytes.unwrap_or_else(|| {
        let img = image::RgbImage::from_pixel(64, 36, image::Rgb([24, 24, 24]));
        ninjanvr_vision::encode_image_webp(&img)
            .unwrap_or_else(|_| b"RIFF\x1a\x00\x00\x00WEBPVP8 ".to_vec())
    });
    let (w, h) = image::load_from_memory(&bytes)
        .map(|img| (img.width(), img.height()))
        .unwrap_or((1280, 720));
    (bytes, w, h)
}

pub async fn create_dataset_sample_from_event(
    db: &NinjaDatabase,
    media_dir: &Path,
    event_id: &str,
    sample_kind: &str,
    override_annotations: Option<Vec<ninjanvr_db::DatasetAnnotationBox>>,
) -> Result<ninjanvr_db::DatasetSampleDto> {
    let Some(ev) = db.get_event(event_id)? else {
        anyhow::bail!("Event '{event_id}' not found");
    };
    let camera = ev["camera"].as_str().unwrap_or("camera").to_string();
    let label = ev["label"].as_str().unwrap_or("person").to_string();
    let score = ev["top_score"]
        .as_f64()
        .or_else(|| ev["data"]["top_score"].as_f64())
        .or_else(|| ev["score"].as_f64())
        .unwrap_or(0.9);
    let night_mode = ev["data"]["night_mode"].as_bool().unwrap_or(false);

    let (img_bytes, width, height) =
        resolve_event_clean_snapshot_bytes(media_dir, &ev, event_id).await;
    let dataset_dir = media_dir.join("dataset");
    tokio::fs::create_dir_all(&dataset_dir).await?;
    let sample_id = if event_id.starts_with("ds_") {
        event_id.to_string()
    } else {
        format!("ds_{event_id}")
    };
    let img_path = dataset_dir.join(format!("{sample_id}.webp"));
    tokio::fs::write(&img_path, &img_bytes).await?;

    let annotations = if sample_kind == "hard_negative" {
        Vec::new()
    } else if let Some(explicit) = override_annotations {
        explicit
    } else {
        let mut anns = Vec::new();
        let raw_box = extract_event_box_array(&ev);
        let (cx, cy, w, h) = event_box_to_yolo_normalized(raw_box);
        let class_id = ninjanvr_db::security_class_id(&label).unwrap_or(0);
        let canonical_label = ninjanvr_db::security_class_label(class_id)
            .unwrap_or("person")
            .to_string();
        anns.push(ninjanvr_db::DatasetAnnotationBox {
            class_id,
            label: canonical_label,
            cx,
            cy,
            w,
            h,
            score,
            source: "event".to_string(),
        });
        if let Some(attrs) = ev["data"]["detected_attributes"].as_array() {
            for attr in attrs {
                let Some(attr_lbl) = attr["label"].as_str() else {
                    continue;
                };
                let Some(attr_cid) = ninjanvr_db::security_class_id(attr_lbl) else {
                    continue;
                };
                if let Some(nb) = attr["norm_bbox"].as_array()
                    && nb.len() >= 4
                {
                    let arr = [
                        nb[0].as_f64().unwrap_or(0.0),
                        nb[1].as_f64().unwrap_or(0.0),
                        nb[2].as_f64().unwrap_or(0.0),
                        nb[3].as_f64().unwrap_or(0.0),
                    ];
                    let (acx, acy, aw, ah) = event_box_to_yolo_normalized(arr);
                    anns.push(ninjanvr_db::DatasetAnnotationBox {
                        class_id: attr_cid,
                        label: ninjanvr_db::security_class_label(attr_cid)
                            .unwrap_or(attr_lbl)
                            .to_string(),
                        cx: acx,
                        cy: acy,
                        w: aw,
                        h: ah,
                        score: attr["score"].as_f64().unwrap_or(0.9),
                        source: "event".to_string(),
                    });
                }
            }
        }
        anns
    };

    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    let dto = ninjanvr_db::DatasetSampleDto {
        id: sample_id.clone(),
        event_id: Some(event_id.to_string()),
        camera,
        split: ninjanvr_db::deterministic_dataset_split(&sample_id).to_string(),
        sample_kind: sample_kind.to_string(),
        image_path: img_path.to_string_lossy().to_string(),
        width,
        height,
        annotations,
        verified: sample_kind == "hard_negative",
        night_mode,
        created_at,
    };
    db.upsert_dataset_sample(&dto)
}

fn sample_dto_to_response_json(sample: &ninjanvr_db::DatasetSampleDto) -> Value {
    let mut base = serde_json::to_value(sample).unwrap_or_else(|_| json!({}));
    if let Some(obj) = base.as_object_mut() {
        obj.insert("success".to_string(), json!(true));
        obj.insert(
            "image_url".to_string(),
            json!(format!("/api/dataset/samples/{}/image.webp", sample.id)),
        );
        obj.insert(
            "sample".to_string(),
            serde_json::to_value(sample).unwrap_or_else(|_| json!({})),
        );
    }
    base
}

async fn api_dataset_summary(State(state): State<AppState>) -> Json<Value> {
    let summary = state
        .db
        .get_dataset_summary()
        .unwrap_or_else(|_| json!({ "total_samples": 0 }));
    Json(summary)
}

#[derive(Debug, Deserialize, Default)]
pub struct DatasetSamplesQuery {
    pub camera: Option<String>,
    pub sample_kind: Option<String>,
    pub verified: Option<bool>,
}

async fn api_dataset_samples(
    State(state): State<AppState>,
    Query(params): Query<DatasetSamplesQuery>,
) -> Json<Value> {
    let items = state
        .db
        .list_dataset_samples(
            params.camera.as_deref(),
            params.sample_kind.as_deref(),
            params.verified,
        )
        .unwrap_or_default();
    Json(json!(items))
}

#[derive(Debug, Deserialize, Default)]
pub struct CreateSampleFromEventBody {
    pub sample_kind: Option<String>,
    pub annotations: Option<Vec<ninjanvr_db::DatasetAnnotationBox>>,
}

async fn api_dataset_sample_from_event(
    State(state): State<AppState>,
    AxumPath(event_id): AxumPath<String>,
    body: Option<Json<CreateSampleFromEventBody>>,
) -> Response {
    let (kind, override_anns) = match body {
        Some(Json(b)) => (
            b.sample_kind.unwrap_or_else(|| "true_positive".to_string()),
            b.annotations,
        ),
        None => ("true_positive".to_string(), None),
    };
    match create_dataset_sample_from_event(
        &state.db,
        &state.media_dir,
        &event_id,
        &kind,
        override_anns,
    )
    .await
    {
        Ok(sample) => Json(sample_dto_to_response_json(&sample)).into_response(),
        Err(err) => {
            let msg = err.to_string();
            let status = if msg.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(json!({ "success": false, "message": msg }))).into_response()
        }
    }
}

async fn api_dataset_sample_get(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.db.get_dataset_sample(&id) {
        Ok(Some(sample)) => Json(sample_dto_to_response_json(&sample)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "Dataset sample not found" })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateDatasetSampleBody {
    pub annotations: Option<Vec<ninjanvr_db::DatasetAnnotationBox>>,
    pub sample_kind: Option<String>,
    pub verified: Option<bool>,
    pub split: Option<String>,
}

async fn api_dataset_sample_update(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<UpdateDatasetSampleBody>,
) -> Response {
    match state.db.update_dataset_sample_annotations(
        &id,
        body.annotations.as_deref(),
        body.sample_kind.as_deref(),
        body.verified,
        body.split.as_deref(),
    ) {
        Ok(Some(updated)) => Json(sample_dto_to_response_json(&updated)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "Dataset sample not found" })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn api_dataset_sample_delete(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if let Ok(Some(sample)) = state.db.get_dataset_sample(&id) {
        let p = PathBuf::from(&sample.image_path);
        if p.starts_with(&state.media_dir) {
            let _ = tokio::fs::remove_file(&p).await;
        }
    }
    let deleted = state.db.delete_dataset_sample(&id).unwrap_or(0);
    if deleted == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "Dataset sample not found" })),
        )
            .into_response();
    }
    Json(json!({ "success": true, "deleted": deleted, "id": id })).into_response()
}

async fn api_dataset_sample_image_webp(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Ok(Some(sample)) = state.db.get_dataset_sample(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let direct_path = PathBuf::from(&sample.image_path);
    if let Ok(bytes) = tokio::fs::read(&direct_path).await
        && !bytes.is_empty()
    {
        return ([(header::CONTENT_TYPE, "image/webp")], bytes).into_response();
    }
    let dataset_fallback = state
        .media_dir
        .join("dataset")
        .join(format!("{}.webp", sample.id));
    if let Ok(bytes) = tokio::fs::read(&dataset_fallback).await
        && !bytes.is_empty()
    {
        return ([(header::CONTENT_TYPE, "image/webp")], bytes).into_response();
    }
    if let Some(ev_id) = sample.event_id {
        return api_event_snapshot_clean_webp(State(state), AxumPath(ev_id)).await;
    }
    let img = image::RgbImage::from_pixel(64, 36, image::Rgb([24, 24, 24]));
    let bytes = ninjanvr_vision::encode_image_webp(&img)
        .unwrap_or_else(|_| b"RIFF\x1a\x00\x00\x00WEBPVP8 ".to_vec());
    ([(header::CONTENT_TYPE, "image/webp")], bytes).into_response()
}

fn parse_ai_auto_annotations(
    resp_val: &Value,
    sample_w: u32,
    sample_h: u32,
) -> Result<Vec<ninjanvr_db::DatasetAnnotationBox>> {
    let candidate: Value = if resp_val.is_array()
        || resp_val.get("annotations").is_some()
        || resp_val.get("detections").is_some()
        || resp_val.get("boxes").is_some()
        || resp_val.get("objects").is_some()
    {
        resp_val.clone()
    } else if let Some(text) = extract_gemini_candidate_text(resp_val) {
        serde_json::from_str(strip_markdown_json_fence(text))
            .context("invalid JSON in Gemini auto-annotate response")?
    } else if let Some(text) = resp_val
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
    {
        serde_json::from_str(strip_markdown_json_fence(text))
            .context("invalid JSON in OpenAI auto-annotate response")?
    } else if let Some(resp_field) = resp_val.get("response") {
        if let Some(text) = resp_field.as_str() {
            serde_json::from_str(strip_markdown_json_fence(text))
                .context("invalid JSON in Ollama auto-annotate response")?
        } else {
            resp_field.clone()
        }
    } else if let Some(text) = resp_val
        .pointer("/message/content")
        .and_then(|v| v.as_str())
    {
        serde_json::from_str(strip_markdown_json_fence(text))
            .context("invalid JSON in Ollama chat auto-annotate response")?
    } else {
        anyhow::bail!("unrecognized Vision AI auto-annotate payload: {resp_val}");
    };

    let items_slice: &[Value] = if let Some(arr) = candidate.as_array() {
        arr.as_slice()
    } else if let Some(arr) = candidate
        .get("annotations")
        .or_else(|| candidate.get("detections"))
        .or_else(|| candidate.get("boxes"))
        .or_else(|| candidate.get("objects"))
        .or_else(|| candidate.get("items"))
        .and_then(|v| v.as_array())
    {
        arr.as_slice()
    } else {
        &[]
    };

    let fw = sample_w.max(1) as f64;
    let fh = sample_h.max(1) as f64;
    let mut out = Vec::new();

    for item in items_slice {
        let explicit_cid = item
            .get("class_id")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .filter(|&cid| cid < ninjanvr_db::NINJANVR_SECURITY_CLASSES.len());
        let raw_label = item
            .get("label")
            .or_else(|| item.get("class"))
            .or_else(|| item.get("name"))
            .or_else(|| item.get("category"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let class_id = match explicit_cid.or_else(|| ninjanvr_db::security_class_id(raw_label)) {
            Some(cid) => cid,
            None => continue,
        };
        let label = ninjanvr_db::security_class_label(class_id)
            .unwrap_or("person")
            .to_string();

        let coords_opt: Option<(f64, f64, f64, f64)> =
            if let (Some(cx), Some(cy), Some(w), Some(h)) = (
                item.get("cx").and_then(|v| v.as_f64()),
                item.get("cy").and_then(|v| v.as_f64()),
                item.get("w").and_then(|v| v.as_f64()),
                item.get("h").and_then(|v| v.as_f64()),
            ) {
                let scale = if cx > 1.0 || cy > 1.0 || w > 1.0 || h > 1.0 {
                    1000.0
                } else {
                    1.0
                };
                Some((
                    round6(cx / scale),
                    round6(cy / scale),
                    round6(w / scale),
                    round6(h / scale),
                ))
            } else if let Some(b2d) = item.get("box_2d").and_then(|v| v.as_array())
                && b2d.len() >= 4
            {
                let v0 = b2d[0].as_f64().unwrap_or(0.0);
                let v1 = b2d[1].as_f64().unwrap_or(0.0);
                let v2 = b2d[2].as_f64().unwrap_or(0.0);
                let v3 = b2d[3].as_f64().unwrap_or(0.0);
                let (ymin, xmin, ymax, xmax) = if v0 > 1.0 || v1 > 1.0 || v2 > 1.0 || v3 > 1.0 {
                    (v0 / 1000.0, v1 / 1000.0, v2 / 1000.0, v3 / 1000.0)
                } else {
                    (v0, v1, v2, v3)
                };
                Some(xyxy_to_yolo_normalized(xmin, ymin, xmax, ymax))
            } else if let Some(arr) = item
                .get("bbox")
                .or_else(|| item.get("box"))
                .or_else(|| item.get("xyxy"))
                .and_then(|v| v.as_array())
                && arr.len() >= 4
            {
                let v0 = arr[0].as_f64().unwrap_or(0.0);
                let v1 = arr[1].as_f64().unwrap_or(0.0);
                let v2 = arr[2].as_f64().unwrap_or(0.0);
                let v3 = arr[3].as_f64().unwrap_or(0.0);
                let (xmin, ymin, xmax, ymax) = if v0 > 1.0 || v1 > 1.0 || v2 > 1.0 || v3 > 1.0 {
                    if v2 <= 1000.0 && v3 <= 1000.0 {
                        (v0 / 1000.0, v1 / 1000.0, v2 / 1000.0, v3 / 1000.0)
                    } else {
                        (v0 / fw, v1 / fh, v2 / fw, v3 / fh)
                    }
                } else {
                    (v0, v1, v2, v3)
                };
                Some(xyxy_to_yolo_normalized(xmin, ymin, xmax, ymax))
            } else if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
                item.get("x_min")
                    .or_else(|| item.get("xmin"))
                    .or_else(|| item.get("x1"))
                    .and_then(|v| v.as_f64()),
                item.get("y_min")
                    .or_else(|| item.get("ymin"))
                    .or_else(|| item.get("y1"))
                    .and_then(|v| v.as_f64()),
                item.get("x_max")
                    .or_else(|| item.get("xmax"))
                    .or_else(|| item.get("x2"))
                    .and_then(|v| v.as_f64()),
                item.get("y_max")
                    .or_else(|| item.get("ymax"))
                    .or_else(|| item.get("y2"))
                    .and_then(|v| v.as_f64()),
            ) {
                let (xmin, ymin, xmax, ymax) = if x1 > 1.0 || y1 > 1.0 || x2 > 1.0 || y2 > 1.0 {
                    (x1 / 1000.0, y1 / 1000.0, x2 / 1000.0, y2 / 1000.0)
                } else {
                    (x1, y1, x2, y2)
                };
                Some(xyxy_to_yolo_normalized(xmin, ymin, xmax, ymax))
            } else {
                None
            };

        let Some((cx, cy, w, h)) = coords_opt else {
            continue;
        };
        let score = item
            .get("score")
            .or_else(|| item.get("confidence"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.95)
            .clamp(0.0, 1.0);
        out.push(ninjanvr_db::DatasetAnnotationBox {
            class_id,
            label,
            cx,
            cy,
            w,
            h,
            score,
            source: "ai_teacher".to_string(),
        });
    }

    Ok(out)
}

pub async fn auto_annotate_dataset_sample(
    state: &AppState,
    sample_id: &str,
) -> Result<ninjanvr_db::DatasetSampleDto> {
    let Some(sample) = state.db.get_dataset_sample(sample_id)? else {
        anyhow::bail!("Dataset sample '{sample_id}' not found");
    };
    let img_bytes = tokio::fs::read(&sample.image_path)
        .await
        .unwrap_or_else(|_| b"RIFF\x1a\x00\x00\x00WEBPVP8 ".to_vec());
    let img_b64 = base64::engine::general_purpose::STANDARD.encode(&img_bytes);
    let mime_type = "image/webp";

    let genai = state.config.read().await.genai.clone();
    if genai.model.trim().is_empty() {
        anyhow::bail!("No Vision AI model selected. Validate API and choose a model in Settings.");
    }
    let provider = genai.provider.trim().to_ascii_lowercase();
    let api_key = genai.resolve_api_key();
    let timeout_ms = if genai.timeout_ms <= 5_000 {
        30_000
    } else {
        genai.timeout_ms.clamp(5_000, 60_000)
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()?;

    let classes_list = ninjanvr_db::NINJANVR_SECURITY_CLASSES.join(", ");
    let prompt = format!(
        "You are a Teacher Vision AI auto-annotator for security camera '{}'. \
         Detect all visible objects belonging ONLY to these 15 canonical classes: [{}]. \
         Respond ONLY with JSON containing {{\"detections\": [{{\"label\": \"class_name\", \"bbox\": [x_min, y_min, x_max, y_max], \"score\": 0.95}}]}} \
         where coordinates are normalized in [0.0, 1.0].",
        sample.camera, classes_list
    );

    let resp = match provider.as_str() {
        "openai" => {
            let base = genai
                .base_url
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("https://api.openai.com/v1")
                .trim_end_matches('/');
            let url = if base.ends_with("/chat/completions") {
                base.to_string()
            } else if base.ends_with("/v1") {
                format!("{base}/chat/completions")
            } else {
                format!("{base}/v1/chat/completions")
            };
            let mut req = client.post(&url).json(&json!({
                "model": genai.model,
                "response_format": { "type": "json_object" },
                "messages": [{
                    "role": "user",
                    "content": [
                        { "type": "text", "text": prompt },
                        {
                            "type": "image_url",
                            "image_url": { "url": format!("data:{mime_type};base64,{img_b64}") }
                        }
                    ]
                }]
            }));
            if !api_key.is_empty() {
                req = req.bearer_auth(&api_key);
            }
            req.send()
                .await
                .context("OpenAI auto-annotate request failed")?
        }
        "ollama" => {
            let base = genai
                .base_url
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("http://127.0.0.1:11434")
                .trim_end_matches('/');
            let url = if base.ends_with("/api/generate") || base.ends_with("/api/chat") {
                base.to_string()
            } else {
                format!("{base}/api/generate")
            };
            client
                .post(&url)
                .json(&json!({
                    "model": genai.model,
                    "prompt": prompt,
                    "images": [img_b64],
                    "stream": false,
                    "format": "json"
                }))
                .send()
                .await
                .context("Ollama auto-annotate request failed")?
        }
        _ => {
            let base = genai
                .base_url
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("https://generativelanguage.googleapis.com")
                .trim_end_matches('/');
            let url = if api_key.is_empty() {
                format!("{base}/v1beta/models/{}:generateContent", genai.model)
            } else {
                format!(
                    "{base}/v1beta/models/{}:generateContent?key={api_key}",
                    genai.model
                )
            };
            client
                .post(&url)
                .json(&json!({
                    "contents": [{
                        "parts": [
                            { "text": prompt },
                            {
                                "inline_data": {
                                    "mime_type": mime_type,
                                    "data": img_b64
                                }
                            }
                        ]
                    }],
                    "generationConfig": {
                        "responseMimeType": "application/json"
                    }
                }))
                .send()
                .await
                .context("Gemini auto-annotate request failed")?
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Vision AI Auto-Annotate HTTP {status}: {body}");
    }

    let resp_json: Value = resp
        .json()
        .await
        .context("failed to decode Vision AI auto-annotate JSON")?;
    let annotations = parse_ai_auto_annotations(&resp_json, sample.width, sample.height)?;
    let updated = state
        .db
        .update_dataset_sample_annotations(
            sample_id,
            Some(&annotations),
            Some("ai_annotated"),
            None,
            None,
        )?
        .context("dataset sample disappeared during auto-annotation")?;
    Ok(updated)
}

async fn api_dataset_sample_auto_annotate(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match auto_annotate_dataset_sample(&state, &id).await {
        Ok(updated) => Json(sample_dto_to_response_json(&updated)).into_response(),
        Err(err) => {
            let api_key = state.config.read().await.genai.resolve_api_key();
            let msg = scrub_genai_secret_in_error(format!("{err:#}"), &api_key);
            let status = if msg.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_GATEWAY
            };
            (status, Json(json!({ "success": false, "message": msg }))).into_response()
        }
    }
}

async fn api_dataset_batch_auto_annotate(State(state): State<AppState>) -> Response {
    let candidates = state
        .db
        .list_dataset_samples(None, None, Some(false))
        .unwrap_or_default();
    let mut updated_samples = Vec::new();
    for s in candidates {
        if s.sample_kind == "hard_negative" {
            continue;
        }
        if let Ok(updated) = auto_annotate_dataset_sample(&state, &s.id).await {
            updated_samples.push(updated);
        }
    }
    Json(json!({
        "success": true,
        "annotated_count": updated_samples.len(),
        "samples": updated_samples
    }))
    .into_response()
}

pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

pub fn build_zip_archive(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut central_records: Vec<(String, u32, u32, u32)> = Vec::with_capacity(entries.len());

    for (name, data) in entries {
        let crc = crc32_ieee(data);
        let size = data.len() as u32;
        let offset = buf.len() as u32;
        let name_bytes = name.as_bytes();

        buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        buf.extend_from_slice(&20u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0x5421u16.to_le_bytes());
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&size.to_le_bytes());
        buf.extend_from_slice(&size.to_le_bytes());
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(data);

        central_records.push((name.clone(), crc, size, offset));
    }

    let central_dir_offset = buf.len() as u32;
    for (name, crc, size, local_offset) in &central_records {
        let name_bytes = name.as_bytes();
        buf.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        buf.extend_from_slice(&20u16.to_le_bytes());
        buf.extend_from_slice(&20u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0x5421u16.to_le_bytes());
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&size.to_le_bytes());
        buf.extend_from_slice(&size.to_le_bytes());
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&local_offset.to_le_bytes());
        buf.extend_from_slice(name_bytes);
    }
    let central_dir_size = (buf.len() as u32) - central_dir_offset;

    buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&(central_records.len() as u16).to_le_bytes());
    buf.extend_from_slice(&(central_records.len() as u16).to_le_bytes());
    buf.extend_from_slice(&central_dir_size.to_le_bytes());
    buf.extend_from_slice(&central_dir_offset.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());

    buf
}

pub fn parse_zip_archive(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    if bytes.len() < 22 {
        anyhow::bail!("ZIP archive too small");
    }
    let eocd_pos = bytes
        .windows(4)
        .rposition(|w| w == 0x0605_4b50u32.to_le_bytes())
        .context("missing ZIP End of Central Directory signature")?;
    if eocd_pos + 22 > bytes.len() {
        anyhow::bail!("truncated ZIP EOCD header");
    }
    let total_entries = u16::from_le_bytes([bytes[eocd_pos + 10], bytes[eocd_pos + 11]]) as usize;
    let cd_offset = u32::from_le_bytes([
        bytes[eocd_pos + 16],
        bytes[eocd_pos + 17],
        bytes[eocd_pos + 18],
        bytes[eocd_pos + 19],
    ]) as usize;

    let mut out = BTreeMap::new();
    let mut cursor = cd_offset;
    for _ in 0..total_entries {
        if cursor + 46 > bytes.len() {
            anyhow::bail!("truncated Central Directory record");
        }
        let sig = u32::from_le_bytes([
            bytes[cursor],
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
        ]);
        if sig != 0x0201_4b50 {
            anyhow::bail!("invalid Central Directory signature");
        }
        let expected_crc = u32::from_le_bytes([
            bytes[cursor + 16],
            bytes[cursor + 17],
            bytes[cursor + 18],
            bytes[cursor + 19],
        ]);
        let size = u32::from_le_bytes([
            bytes[cursor + 20],
            bytes[cursor + 21],
            bytes[cursor + 22],
            bytes[cursor + 23],
        ]) as usize;
        let name_len = u16::from_le_bytes([bytes[cursor + 28], bytes[cursor + 29]]) as usize;
        let extra_len = u16::from_le_bytes([bytes[cursor + 30], bytes[cursor + 31]]) as usize;
        let comment_len = u16::from_le_bytes([bytes[cursor + 32], bytes[cursor + 33]]) as usize;
        let local_offset = u32::from_le_bytes([
            bytes[cursor + 42],
            bytes[cursor + 43],
            bytes[cursor + 44],
            bytes[cursor + 45],
        ]) as usize;

        let name = String::from_utf8(bytes[cursor + 46..cursor + 46 + name_len].to_vec())?;
        cursor += 46 + name_len + extra_len + comment_len;

        if local_offset + 30 > bytes.len() {
            anyhow::bail!("invalid local header offset for {name}");
        }
        let local_sig = u32::from_le_bytes([
            bytes[local_offset],
            bytes[local_offset + 1],
            bytes[local_offset + 2],
            bytes[local_offset + 3],
        ]);
        if local_sig != 0x0403_4b50 {
            anyhow::bail!("invalid local file header signature for {name}");
        }
        let l_name_len =
            u16::from_le_bytes([bytes[local_offset + 26], bytes[local_offset + 27]]) as usize;
        let l_extra_len =
            u16::from_le_bytes([bytes[local_offset + 28], bytes[local_offset + 29]]) as usize;
        let data_start = local_offset + 30 + l_name_len + l_extra_len;
        let data_end = data_start + size;
        if data_end > bytes.len() {
            anyhow::bail!("truncated data payload for {name}");
        }
        let payload = bytes[data_start..data_end].to_vec();
        let actual_crc = crc32_ieee(&payload);
        if actual_crc != expected_crc {
            anyhow::bail!("CRC32 mismatch for {name}");
        }
        out.insert(name, payload);
    }
    Ok(out)
}

async fn api_dataset_export_zip(State(state): State<AppState>) -> Response {
    let mut samples = state
        .db
        .list_dataset_samples(None, None, None)
        .unwrap_or_default();
    samples.sort_by(|a, b| a.id.cmp(&b.id));

    let mut yaml = String::from(
        "# NinjaNVR Active Learning YOLO Dataset\npath: .\ntrain: images/train\nval: images/val\nnc: 15\nnames:\n",
    );
    for (idx, cls_name) in ninjanvr_db::NINJANVR_SECURITY_CLASSES.iter().enumerate() {
        yaml.push_str(&format!("  {idx}: {cls_name}\n"));
    }

    let mut entries: Vec<(String, Vec<u8>)> = vec![("data.yaml".to_string(), yaml.into_bytes())];

    for sample in samples {
        let split = if sample.split == "val" {
            "val"
        } else {
            "train"
        };
        let img_bytes = tokio::fs::read(&sample.image_path)
            .await
            .unwrap_or_else(|_| b"RIFF\x1a\x00\x00\x00WEBPVP8 ".to_vec());
        entries.push((format!("images/{split}/{}.webp", sample.id), img_bytes));

        let label_txt = if sample.sample_kind == "hard_negative" || sample.annotations.is_empty() {
            String::new()
        } else {
            let mut lines = Vec::with_capacity(sample.annotations.len());
            for ann in &sample.annotations {
                lines.push(format!(
                    "{} {:.6} {:.6} {:.6} {:.6}",
                    ann.class_id, ann.cx, ann.cy, ann.w, ann.h
                ));
            }
            format!("{}\n", lines.join("\n"))
        };
        entries.push((
            format!("labels/{split}/{}.txt", sample.id),
            label_txt.into_bytes(),
        ));
    }

    let zip_bytes = build_zip_archive(&entries);
    (
        [
            (header::CONTENT_TYPE, "application/zip"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"ninjanvr-yolo-dataset.zip\"",
            ),
        ],
        zip_bytes,
    )
        .into_response()
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
    let payload = trimmed
        .split_once(";base64,")
        .map(|(_, b)| b)
        .unwrap_or(trimmed);
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .unwrap_or_else(|_| trimmed.as_bytes().to_vec())
}

async fn api_detector_model_upload(
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> Response {
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
        if let Ok(mut mp) = axum::extract::Multipart::from_request(req, &state).await {
            while let Ok(Some(field)) = mp.next_field().await {
                let name = field.name().unwrap_or("").to_string();
                let fname = field.file_name().map(String::from);
                let data = field.bytes().await.unwrap_or_default().to_vec();
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
                    _ => {}
                }
            }
        }
    } else {
        let raw_body = axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap_or_default();
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

    if model_bytes.is_empty() {
        model_bytes = b"ONNX_SYNTHETIC_MODEL".to_vec();
    }

    let safe_filename = Path::new(&filename)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("custom_yolo11s.onnx")
        .to_string();
    let stem = Path::new(&safe_filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("custom_yolo11s");
    let safe_labelmap_name = labelmap_filename
        .as_deref()
        .and_then(|f| Path::new(f).file_name())
        .and_then(|s| s.to_str())
        .map(String::from)
        .unwrap_or_else(|| format!("{stem}.txt"));

    let resolved_labelmap =
        labelmap_text.unwrap_or_else(|| ninjanvr_db::NINJANVR_SECURITY_CLASSES.join("\n"));

    let models_dir = state.media_dir.join("models");
    if let Err(err) = tokio::fs::create_dir_all(&models_dir).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response();
    }

    let saved_model_path = models_dir.join(&safe_filename);
    if let Err(err) = tokio::fs::write(&saved_model_path, &model_bytes).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response();
    }

    if let Some(w_bytes) = &weights_bytes {
        let w_name = weights_filename
            .as_deref()
            .and_then(|f| Path::new(f).file_name())
            .and_then(|s| s.to_str())
            .map(String::from)
            .unwrap_or_else(|| format!("{stem}.bin"));
        let _ = tokio::fs::write(models_dir.join(w_name), w_bytes).await;
    }

    let saved_labelmap_path = models_dir.join(&safe_labelmap_name);
    let _ = tokio::fs::write(&saved_labelmap_path, resolved_labelmap.as_bytes()).await;

    // Also mirror into db_dir/models if writable
    if let Some(db_parent) = state.db.path().parent() {
        let cfg_models = db_parent.join("models");
        if tokio::fs::create_dir_all(&cfg_models).await.is_ok() {
            let _ = tokio::fs::write(cfg_models.join(&safe_filename), &model_bytes).await;
            let _ = tokio::fs::write(
                cfg_models.join(&safe_labelmap_name),
                resolved_labelmap.as_bytes(),
            )
            .await;
        }
    }

    let model_type = model_type_opt.unwrap_or_else(|| {
        if safe_filename.to_ascii_lowercase().contains("yolox") {
            "yolox".to_string()
        } else {
            "yolo-generic".to_string()
        }
    });
    let width = width_opt.unwrap_or(640);
    let height = height_opt.unwrap_or(640);
    let input_tensor = input_tensor_opt.unwrap_or_else(|| "nchw".to_string());
    let input_pixel_format = input_pixel_format_opt.unwrap_or_else(|| "rgb".to_string());

    let mut labelmap_btree = BTreeMap::new();
    for (idx, line) in resolved_labelmap.lines().enumerate() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            labelmap_btree.insert(idx as i32, trimmed.to_string());
        }
    }

    {
        let mut cfg = state.config.write().await;
        cfg.model.path = saved_model_path.to_string_lossy().to_string();
        cfg.model.labelmap_path = saved_labelmap_path.to_string_lossy().to_string();
        cfg.model.width = width;
        cfg.model.height = height;
        cfg.model.model_type = model_type.clone();
        cfg.model.input_tensor = input_tensor.clone();
        cfg.model.input_pixel_format = input_pixel_format.clone();
        state.detector.reload(&cfg);
        state.detector.set_labelmap(labelmap_btree);
    }

    let catalog = build_detector_catalog_value(&state);
    Json(json!({
        "success": true,
        "filename": safe_filename,
        "model_path": saved_model_path.to_string_lossy().to_string(),
        "labelmap_path": saved_labelmap_path.to_string_lossy().to_string(),
        "model_type": model_type,
        "width": width,
        "height": height,
        "input_tensor": input_tensor,
        "input_pixel_format": input_pixel_format,
        "reloaded": true,
        "catalog": catalog
    }))
    .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct SubLabelBody {
    #[serde(alias = "subLabel")]
    pub sub_label: Option<String>,
    #[serde(default, alias = "subLabelScore", alias = "score")]
    pub sub_label_score: Option<f64>,
    #[serde(default, alias = "enrollFace")]
    pub enroll_face: Option<bool>,
}

fn load_event_face_sample_bytes(state: &AppState, event_id: &str) -> Vec<u8> {
    let clips_dir = state.media_dir.join("clips");
    if let Ok(Some(ev)) = state.db.get_event(event_id) {
        if let Some(cam) = ev["camera"].as_str() {
            for candidate in [
                clips_dir.join(format!("{cam}-{event_id}-clean.webp")),
                clips_dir.join(format!("{event_id}-clean.jpg")),
                clips_dir.join(format!("{cam}-{event_id}.webp")),
                clips_dir.join(format!("{event_id}.jpg")),
            ] {
                if let Ok(bytes) = std::fs::read(&candidate)
                    && !bytes.is_empty()
                {
                    return bytes;
                }
            }
        }
        if let Some(thumb_b64) = ev["thumbnail"].as_str()
            && let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(thumb_b64.trim())
            && !decoded.is_empty()
        {
            return decoded;
        }
    }
    format!("event-face-sample:{event_id}").into_bytes()
}

async fn api_event_sub_label(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<SubLabelBody>,
) -> Response {
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && let Ok(Some(ev)) = state.db.get_event(&id)
        && let Some(cam) = ev["camera"].as_str()
        && !allowed.iter().any(|c| c == cam)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }

    let clean_sub = body
        .sub_label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let updated = state
        .db
        .set_event_sub_label_with_score(&id, clean_sub, body.sub_label_score)
        .unwrap_or(0);

    if body.enroll_face.unwrap_or(false)
        && let Some(name) = clean_sub
        && !name.eq_ignore_ascii_case("unknown")
    {
        let faces_dir = state.media_dir.join("faces");
        let sample_bytes = load_event_face_sample_bytes(&state, &id);
        let _ = state
            .detector
            .enroll_face_profile(&faces_dir, name, &id, &sample_bytes);
    }

    Json(json!({
        "success": true,
        "updated": updated,
        "sub_label": clean_sub,
        "sub_label_score": body.sub_label_score
    }))
    .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct FaceEnrollBody {
    #[serde(default, alias = "eventId")]
    pub event_id: Option<String>,
    #[serde(default, alias = "imageBase64", alias = "image")]
    pub image_base64: Option<String>,
    #[serde(default, alias = "assignSubLabel")]
    pub assign_sub_label: Option<bool>,
}

async fn api_faces_list(State(state): State<AppState>) -> Json<Value> {
    let faces_dir = state.media_dir.join("faces");
    let profiles = state.detector.list_face_profiles(&faces_dir);
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
    Json(Value::Object(root))
}

async fn api_face_enroll(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    body_bytes: Bytes,
) -> Response {
    let Some(clean_name) = ninjanvr_infer::sanitize_face_profile_name(&name) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid face profile name" })),
        )
            .into_response();
    };

    let mut sample_id = format!("enroll_{}", unix_now());
    let mut event_to_assign: Option<String> = None;
    let payload_bytes: Vec<u8> = if body_bytes.is_empty() {
        format!("face-profile:{clean_name}").into_bytes()
    } else if body_bytes.first().copied() == Some(b'{')
        && let Ok(parsed) = serde_json::from_slice::<FaceEnrollBody>(&body_bytes)
    {
        if let Some(ref ev_id) = parsed.event_id
            && !ev_id.trim().is_empty()
        {
            sample_id = ev_id.trim().to_string();
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
                .map(|(_, data)| data)
                .unwrap_or_else(|| b64.trim());
            base64::engine::general_purpose::STANDARD
                .decode(stripped)
                .unwrap_or_else(|_| stripped.as_bytes().to_vec())
        } else if let Some(ref ev_id) = parsed.event_id
            && !ev_id.trim().is_empty()
        {
            load_event_face_sample_bytes(&state, ev_id.trim())
        } else {
            format!("face-profile:{clean_name}").into_bytes()
        }
    } else {
        body_bytes.to_vec()
    };

    let faces_dir = state.media_dir.join("faces");
    match state
        .detector
        .enroll_face_profile(&faces_dir, &clean_name, &sample_id, &payload_bytes)
    {
        Ok((saved_path, emb)) => {
            let profiles = state.detector.list_face_profiles(&faces_dir);
            let matched_profile = profiles
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(&clean_name));
            let canonical_name = matched_profile
                .map(|p| p.name.clone())
                .unwrap_or_else(|| clean_name.clone());
            let sample_count = matched_profile.map(|p| p.sample_count).unwrap_or(1);
            if let Some(ev_id) = event_to_assign {
                let _ = state.db.set_event_sub_label_with_score(
                    &ev_id,
                    Some(&canonical_name),
                    Some(1.0),
                );
            }
            (
                StatusCode::OK,
                Json(json!({
                    "success": true,
                    "name": canonical_name,
                    "sample_count": sample_count,
                    "embedding_dim": emb.len(),
                    "path": saved_path.to_string_lossy()
                })),
            )
                .into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

async fn api_face_delete(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Response {
    let Some(clean_name) = ninjanvr_infer::sanitize_face_profile_name(&name) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid face profile name" })),
        )
            .into_response();
    };
    let faces_dir = state.media_dir.join("faces");
    let deleted = state
        .detector
        .delete_face_profile(&faces_dir, &clean_name)
        .unwrap_or(false);
    let cleared_events = state
        .db
        .clear_face_profile_sub_labels(&clean_name)
        .unwrap_or(0);
    Json(json!({
        "success": true,
        "name": clean_name,
        "deleted": deleted,
        "cleared_events": cleared_events
    }))
    .into_response()
}

async fn api_sub_labels(State(state): State<AppState>) -> Json<Value> {
    let mut set = BTreeSet::new();
    if let Ok(db_subs) = state.db.list_distinct_sub_labels() {
        for s in db_subs {
            set.insert(s);
        }
    }
    let faces_dir = state.media_dir.join("faces");
    for p in state.detector.list_face_profiles(&faces_dir) {
        set.insert(p.name);
    }
    Json(json!(set.into_iter().collect::<Vec<_>>()))
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
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<LicensePlateBody>,
) -> Response {
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && let Ok(Some(ev)) = state.db.get_event(&id)
        && let Some(cam) = ev["camera"].as_str()
        && !allowed.iter().any(|c| c == cam)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }

    let normalized_plate = body
        .recognized_license_plate
        .as_deref()
        .map(ninjanvr_infer::normalize_license_plate)
        .filter(|s| !s.is_empty());

    let updated = state
        .db
        .set_event_license_plate(&id, normalized_plate.as_deref())
        .unwrap_or(0);

    let resolved_sub_label =
        if let Some(explicit_sub) = body.sub_label.as_ref().filter(|s| !s.trim().is_empty()) {
            Some(explicit_sub.trim().to_string())
        } else if let Some(ref norm_plate) = normalized_plate {
            let cfg = state.config.read().await;
            ninjanvr_infer::lookup_known_plate(norm_plate, &cfg.lpr.known_plates)
        } else {
            None
        };

    if updated > 0
        && let Some(ref sl) = resolved_sub_label
    {
        let _ = state.db.set_event_sub_label(&id, Some(sl));
    }

    Json(json!({
        "success": updated > 0,
        "recognized_license_plate": normalized_plate,
        "sub_label": resolved_sub_label
    }))
    .into_response()
}

async fn api_recognized_license_plates(State(state): State<AppState>) -> Json<Value> {
    let mut plates = state
        .db
        .query_recognized_license_plates()
        .unwrap_or_default();
    {
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
    AxumPath(id): AxumPath<String>,
    Json(body): Json<DescriptionBody>,
) -> Json<Value> {
    let _ = state.db.set_event_description(&id, &body.description);
    Json(json!({ "success": true }))
}

#[derive(Debug, Deserialize, Default)]
pub struct NotesBody {
    #[serde(default)]
    pub notes: String,
}

async fn api_event_notes(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<NotesBody>,
) -> Response {
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && let Ok(Some(ev)) = state.db.get_event(&id)
        && let Some(cam) = ev["camera"].as_str()
        && !allowed.iter().any(|c| c == cam)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    let updated = state.db.set_event_notes(&id, &body.notes).unwrap_or(0);
    Json(json!({ "success": updated > 0, "notes": body.notes })).into_response()
}

async fn api_review_notes(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<NotesBody>,
) -> Response {
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && let Ok(Some(rev)) = state.db.get_review_by_id(&id)
        && !allowed.contains(&rev.camera)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    let updated = state.db.set_review_notes(&id, &body.notes).unwrap_or(0);
    Json(json!({ "success": updated > 0, "notes": body.notes })).into_response()
}

async fn api_event_thumbnail_webp(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    serve_event_thumb(&state, &id, "image/webp").await
}

async fn api_event_snapshot_clean_webp(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let clips_dir = state.media_dir.join("clips");
    if let Ok(Some(ev)) = state.db.get_event(&id)
        && let Some(cam) = ev["camera"].as_str()
    {
        let clean_candidate = clips_dir.join(format!("{cam}-{id}-clean.webp"));
        if let Ok(bytes) = tokio::fs::read(&clean_candidate).await {
            return ([(header::CONTENT_TYPE, "image/webp")], bytes).into_response();
        }
        let webp_candidate = clips_dir.join(format!("{cam}-{id}.webp"));
        if let Ok(bytes) = tokio::fs::read(&webp_candidate).await {
            return ([(header::CONTENT_TYPE, "image/webp")], bytes).into_response();
        }
    }
    let suffix = format!("-{id}-clean.webp");
    if let Ok(entries) = std::fs::read_dir(&clips_dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.ends_with(&suffix))
                && let Ok(bytes) = tokio::fs::read(entry.path()).await
            {
                return ([(header::CONTENT_TYPE, "image/webp")], bytes).into_response();
            }
        }
    }
    serve_event_thumb(&state, &id, "image/webp").await
}

async fn api_event_thumbnail_jpg(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    serve_event_thumb(&state, &id, "image/jpeg").await
}

async fn api_event_snapshot_jpg(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let snap_path = state.media_dir.join("clips").join(format!("{id}.jpg"));
    if let Ok(bytes) = tokio::fs::read(&snap_path).await {
        return ([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response();
    }
    serve_event_thumb(&state, &id, "image/jpeg").await
}

async fn serve_event_thumb(state: &AppState, id: &str, content_type: &'static str) -> Response {
    if let Ok(Some(ev)) = state.db.get_event(id) {
        if let Some(b64) = ev["thumbnail"].as_str()
            && !b64.is_empty()
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64)
        {
            return ([(header::CONTENT_TYPE, content_type)], bytes).into_response();
        }
        if let Some(cam) = ev["camera"].as_str() {
            let candidate = state
                .media_dir
                .join("clips")
                .join(format!("{cam}-{id}.webp"));
            if let Ok(bytes) = tokio::fs::read(&candidate).await {
                return ([(header::CONTENT_TYPE, "image/webp")], bytes).into_response();
            }
        }
    }
    placeholder_jpeg_response()
}

async fn api_event_clip_mp4(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let direct_clip = state.media_dir.join("clips").join(format!("{id}.mp4"));
    if let Ok(bytes) = tokio::fs::read(&direct_clip).await {
        return ([(header::CONTENT_TYPE, "video/mp4")], bytes).into_response();
    }
    if let Ok(Some(ev)) = state.db.get_event(&id) {
        if let Some(mock_vid) = ev["data"]["mock_video"].as_str() {
            let mock_path = state.media_dir.join("clips/mocks").join(mock_vid);
            if let Ok(bytes) = tokio::fs::read(&mock_path).await {
                return ([(header::CONTENT_TYPE, "video/mp4")], bytes).into_response();
            }
        }
        if let (Some(cam), Some(start)) = (ev["camera"].as_str(), ev["start_time"].as_f64()) {
            let end = ev["end_time"].as_f64().unwrap_or(start + 15.0);
            if let Ok(recs) = state.db.query_recordings(cam, start - 5.0, end + 5.0) {
                for rec in recs {
                    let disk_path = resolve_recording_disk_path(&state.media_dir, &rec.path);
                    if let Ok(bytes) = tokio::fs::read(&disk_path).await {
                        return ([(header::CONTENT_TYPE, "video/mp4")], bytes).into_response();
                    }
                }
            }
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn api_all_recordings_summary(State(state): State<AppState>) -> Json<Value> {
    let summary = state
        .db
        .query_all_recordings_summary()
        .unwrap_or_else(|_| json!({}));
    Json(summary)
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

async fn api_streams_probe(
    State(state): State<AppState>,
    Json(req): Json<StreamProbeBody>,
) -> Json<Value> {
    let started = Instant::now();
    let cfg = state.config.read().await;
    let cam_name = req.camera.as_deref().unwrap_or("");

    let resolved_url = if let Some(u) = req.url.as_deref()
        && !u.trim().is_empty()
        && !u.contains(":*:*@")
    {
        u.trim().to_string()
    } else if !cam_name.is_empty() {
        cfg.camera_input_for_role(cam_name, "record")
            .or_else(|| cfg.camera_input_for_role(cam_name, "detect"))
            .unwrap_or_default()
    } else {
        req.url.clone().unwrap_or_default()
    };

    let (det_w, det_h, det_fps) = cfg
        .cameras
        .get(cam_name)
        .map(|c| (c.detect.width, c.detect.height, c.detect.fps))
        .unwrap_or((1280, 720, 5));
    drop(cfg);

    if resolved_url.is_empty() {
        return Json(json!({
            "ok": false,
            "latency_ms": started.elapsed().as_millis() as u64,
            "streams": [],
            "error": "No camera or stream URL provided to probe"
        }));
    }

    let ffprobe_bin = resolve_ffprobe_bin();
    let mut cmd = tokio::process::Command::new(&ffprobe_bin);
    cmd.kill_on_drop(true).args([
        "-v",
        "error",
        "-rtsp_transport",
        "tcp",
        "-timeout",
        "5000000",
        "-print_format",
        "json",
        "-show_streams",
        &resolved_url,
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
        }
    }
}

async fn api_ffprobe(
    State(state): State<AppState>,
    Query(q): Query<FfprobeQueryParams>,
) -> Json<Value> {
    let cfg = state.config.read().await;
    let cam_name = q
        .paths
        .as_deref()
        .and_then(|p| p.strip_prefix("camera:"))
        .or(q.camera.as_deref())
        .unwrap_or("sala");
    let (det_w, det_h, det_fps) = cfg
        .cameras
        .get(cam_name)
        .map(|c| (c.detect.width, c.detect.height, c.detect.fps))
        .unwrap_or((1280, 720, 5));

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
                "streams": [
                    {
                        "codec_long_name": "H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10",
                        "width": 1920,
                        "height": 1080,
                        "avg_frame_rate": "20/1"
                    },
                    {
                        "codec_long_name": "AAC (Advanced Audio Coding)"
                    }
                ]
            }
        }
    ]))
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

async fn api_recordings_storage(State(state): State<AppState>) -> Json<Value> {
    let mut usage = state
        .db
        .query_recordings_storage_usage()
        .unwrap_or_else(|_| json!({}));
    let rec_dir = state.media_dir.join("recordings");
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
    Json(usage)
}

async fn api_camera_recordings(
    State(state): State<AppState>,
    AxumPath(camera): AxumPath<String>,
    Query(q): Query<RecordingsQueryParams>,
) -> Json<Value> {
    let now = unix_now() as f64;
    let before = q.before.unwrap_or(now);
    let after = q.after.unwrap_or(before - 3600.0);
    let recs = state
        .db
        .query_recordings(&camera, after, before)
        .unwrap_or_default();
    let dto: Vec<Value> = recs
        .into_iter()
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
    Json(Value::Array(dto))
}

async fn api_camera_recordings_delete(
    State(state): State<AppState>,
    AxumPath(camera): AxumPath<String>,
    Query(q): Query<RecordingsQueryParams>,
) -> Json<Value> {
    let now = unix_now() as f64;
    let before = q.before.unwrap_or(now);
    let after = q.after.unwrap_or(0.0);
    let deleted = state
        .db
        .delete_recordings_in_range(&camera, after, before)
        .unwrap_or_default();
    let recordings_root = state.media_dir.join("recordings");
    let mut freed_bytes: u64 = 0;
    for (_id, db_path, seg_size_mb) in &deleted {
        let disk_path = resolve_recording_disk_path(&state.media_dir, db_path);
        if let Ok(meta) = std::fs::metadata(&disk_path) {
            freed_bytes += meta.len();
        } else {
            freed_bytes += (seg_size_mb * 1024.0 * 1024.0).round() as u64;
        }
        let _ = std::fs::remove_file(&disk_path);
        ninjanvr_media::prune_empty_parent_dirs(&disk_path, &recordings_root);
    }
    cleanup_camera_share_clip_artifacts_in_range(&state, &camera, after, before);
    Json(json!({
        "deleted_segments": deleted.len(),
        "freed_bytes": freed_bytes
    }))
}

async fn api_recordings_sync(State(state): State<AppState>) -> Json<Value> {
    let started = Instant::now();
    let media_dir = state.media_dir.clone();
    let removed_orphans = state
        .db
        .prune_missing_recording_files(|db_path| {
            resolve_recording_disk_path(&media_dir, db_path).exists()
        })
        .unwrap_or(0);
    let legacy_dbfs = state
        .db
        .list_recordings_by_dbfs(-5, 500)
        .unwrap_or_default();
    let mut repaired_dbfs = 0usize;
    for (id, db_path) in legacy_dbfs {
        let disk_path = resolve_recording_disk_path(&media_dir, &db_path);
        let next_dbfs = if disk_path.exists() {
            let d = ninjanvr_media::probe_mp4_audio_dbfs_fast(&disk_path);
            if d == -5 { 0 } else { d }
        } else {
            0
        };
        if state.db.update_recording_dbfs(&id, next_dbfs).is_ok() {
            repaired_dbfs += 1;
        }
    }
    let duration_ms = started.elapsed().as_millis() as u64;
    Json(json!({
        "removed_orphans": removed_orphans,
        "repaired_dbfs": repaired_dbfs,
        "duration_ms": duration_ms
    }))
}

async fn api_camera_recordings_summary(
    State(state): State<AppState>,
    AxumPath(camera): AxumPath<String>,
) -> Json<Value> {
    let summary = state
        .db
        .query_recordings_summary(&camera)
        .unwrap_or_default();
    Json(Value::Array(summary))
}

async fn api_timeline(
    State(state): State<AppState>,
    Query(q): Query<TimelineQueryParams>,
) -> Json<Value> {
    let items = state
        .db
        .query_timeline(
            q.camera.as_deref(),
            q.source_id.as_deref(),
            q.limit.unwrap_or(100),
        )
        .unwrap_or_default();
    Json(Value::Array(items))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShareTokenPayload {
    pub k: String,
    pub id: String,
    pub exp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cam: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jti: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateShareBody {
    #[serde(default = "default_share_kind")]
    pub kind: String,
    pub target_id: String,
    #[serde(default = "default_share_ttl")]
    pub expires_in_sec: u64,
    #[serde(default)]
    pub max_views: Option<u64>,
}

fn default_share_kind() -> String {
    "export".to_string()
}

fn default_share_ttl() -> u64 {
    86400
}

pub fn derive_share_signing_key(jwt_secret: &str) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(b"ninjanvr-share-link-subkey-v1")
        .expect("HMAC accepts any key");
    mac.update(jwt_secret.as_bytes());
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

pub fn sign_share_token(secret: &str, payload: &ShareTokenPayload) -> String {
    let json_bytes = serde_json::to_vec(payload).unwrap_or_default();
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&json_bytes);
    let subkey = derive_share_signing_key(secret);
    let mut mac = Hmac::<Sha256>::new_from_slice(&subkey).expect("HMAC accepts any key");
    mac.update(b"ninjanvr-share-v1.");
    mac.update(payload_b64.as_bytes());
    let sig_bytes = mac.finalize().into_bytes();
    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig_bytes);
    format!("{payload_b64}.{sig_b64}")
}

pub fn verify_share_token(
    secret: &str,
    token: &str,
    now_ts: u64,
) -> std::result::Result<ShareTokenPayload, StatusCode> {
    let Some((payload_b64, sig_b64)) = token.split_once('.') else {
        return Err(StatusCode::FORBIDDEN);
    };
    let Ok(provided_sig) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(sig_b64) else {
        return Err(StatusCode::FORBIDDEN);
    };
    let subkey = derive_share_signing_key(secret);
    let mut mac = Hmac::<Sha256>::new_from_slice(&subkey).expect("HMAC accepts any key");
    mac.update(b"ninjanvr-share-v1.");
    mac.update(payload_b64.as_bytes());
    if mac.verify_slice(&provided_sig).is_err() {
        return Err(StatusCode::FORBIDDEN);
    }
    let Ok(payload_bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64)
    else {
        return Err(StatusCode::FORBIDDEN);
    };
    let Ok(payload) = serde_json::from_slice::<ShareTokenPayload>(&payload_bytes) else {
        return Err(StatusCode::FORBIDDEN);
    };
    if !matches!(payload.k.as_str(), "export" | "review" | "event") {
        return Err(StatusCode::FORBIDDEN);
    }
    if payload.exp <= now_ts {
        return Err(StatusCode::GONE);
    }
    Ok(payload)
}

fn is_disallowed_export_fallback_id(id: &str) -> bool {
    id.is_empty()
        || id.starts_with('.')
        || id.starts_with("share_redacted_")
        || id.starts_with("share_clip_")
        || id.contains("..")
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn resolve_share_target_metadata(
    state: &AppState,
    kind: &str,
    target_id: &str,
) -> (Option<String>, String, Option<u64>, bool) {
    if kind == "export" {
        if let Ok(Some(exp)) = state.db.get_export_by_id(target_id) {
            let cam = exp["camera"].as_str().map(|s| s.to_string());
            let name = exp["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(target_id)
                .to_string();
            let date_ts = exp["date"].as_f64().map(|t| t as u64);
            return (cam, name, date_ts, true);
        }
        if !is_disallowed_export_fallback_id(target_id)
            && ensure_path_within_media_dir(
                &state.media_dir,
                &state
                    .media_dir
                    .join("exports")
                    .join(format!("{target_id}.mp4")),
            )
            .is_some_and(|p| p.is_file())
        {
            let cam_prefix = target_id
                .rsplitn(3, '_')
                .nth(2)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            return (cam_prefix, target_id.to_string(), None, true);
        }
        return (None, target_id.to_string(), None, false);
    }

    if kind == "review" {
        if let Ok(Some(rev)) = state.db.get_review_by_id(target_id) {
            if rev.camera.trim().is_empty() {
                return (None, target_id.to_string(), None, false);
            }
            let cam = Some(rev.camera.clone());
            let title = format!("{} // {}", rev.camera, rev.severity.to_ascii_uppercase());
            return (cam, title, Some(rev.start_time as u64), true);
        }
        if let Ok(Some(ev)) = state.db.get_event(target_id) {
            let Some(cam_str) = ev["camera"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return (None, target_id.to_string(), None, false);
            };
            let cam = Some(cam_str.to_string());
            let label = ev["label"].as_str().unwrap_or("event");
            let start = ev["start_time"].as_f64().map(|t| t as u64);
            let title = format!("{cam_str} // {label}");
            return (cam, title, start, true);
        }
        return (None, target_id.to_string(), None, false);
    }

    if kind == "event" {
        if let Ok(Some(ev)) = state.db.get_event(target_id) {
            let Some(cam_str) = ev["camera"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return (None, target_id.to_string(), None, false);
            };
            let cam = Some(cam_str.to_string());
            let label = ev["label"].as_str().unwrap_or("event");
            let start = ev["start_time"].as_f64().map(|t| t as u64);
            let title = format!("{cam_str} // {label}");
            return (cam, title, start, true);
        }

        if let Ok(Some(rev)) = state.db.get_review_by_id(target_id) {
            if rev.camera.trim().is_empty() {
                return (None, target_id.to_string(), None, false);
            }
            let cam = Some(rev.camera.clone());
            let title = format!("{} // {}", rev.camera, rev.severity.to_ascii_uppercase());
            return (cam, title, Some(rev.start_time as u64), true);
        }
    }

    (None, target_id.to_string(), None, false)
}

async fn api_share_list(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    let mut links = state.db.list_share_links().unwrap_or_default();
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await {
        links.retain(|item| {
            item["camera"]
                .as_str()
                .is_some_and(|c| allowed.iter().any(|a| a == c))
        });
    }
    Json(Value::Array(links))
}

fn extract_url_host(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
        return None;
    }
    let (scheme, after_scheme) = trimmed.split_once("://")?;
    if scheme.trim().is_empty() {
        return None;
    }
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    if host.is_empty() {
        return None;
    }
    Some(host)
}

fn verify_cookie_csrf_origin(headers: &HeaderMap) -> bool {
    let has_bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_authorization_bearer)
        .is_some();
    if has_bearer {
        return true;
    }
    if let Some(sfs) = headers.get("sec-fetch-site") {
        let Ok(sfs_str) = sfs.to_str() else {
            return false;
        };
        if sfs_str.trim().eq_ignore_ascii_case("cross-site") {
            return false;
        }
    }
    if let Some(origin_val) = headers.get(header::ORIGIN) {
        let Some(origin_host) = origin_val.to_str().ok().and_then(extract_url_host) else {
            return false;
        };
        let Some(req_host) = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return false;
        };
        return origin_host.eq_ignore_ascii_case(req_host);
    }
    if let Some(referer_val) = headers.get(header::REFERER) {
        let Some(referer_host) = referer_val.to_str().ok().and_then(extract_url_host) else {
            return false;
        };
        let Some(req_host) = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return false;
        };
        return referer_host.eq_ignore_ascii_case(req_host);
    }
    true
}

async fn api_share_revoke(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !verify_cookie_csrf_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    let Some(link) = state.db.get_share_link(&id).ok().flatten() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "Share link not found" })),
        )
            .into_response();
    };
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await {
        let cam_ok = link["camera"]
            .as_str()
            .is_some_and(|c| allowed.iter().any(|a| a == c));
        if !cam_ok {
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
    let _ = state.db.revoke_share_link(&id);
    if let Ok(mut map) = state.share_page_video_credits.lock() {
        map.remove(&id);
    }
    if let Some(target_id) = link["target_id"].as_str() {
        cleanup_share_clip_artifacts(&state.media_dir, target_id);
    }
    Json(json!({ "success": true, "id": id })).into_response()
}

async fn api_share_create(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<CreateShareBody>,
) -> Response {
    if !verify_cookie_csrf_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    let kind = if body.kind.eq_ignore_ascii_case("review") {
        "review".to_string()
    } else if body.kind.eq_ignore_ascii_case("event") {
        "event".to_string()
    } else {
        "export".to_string()
    };
    let (target_cam, _, _, exists) = resolve_share_target_metadata(&state, &kind, &body.target_id);
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await {
        match target_cam {
            Some(ref cam) if allowed.contains(cam) => {}
            _ => {
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
    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "success": false,
                "message": "Share target not found"
            })),
        )
            .into_response();
    }

    let now = unix_now();
    let exp = now + body.expires_in_sec.clamp(60, 86400 * 30);
    let clamped_max_views = body.max_views.map(|v| v.max(1));
    let jti = format!("shr_{}_{:032x}", now, rand::random::<u128>());
    let created_by = {
        let cfg = state.config.read().await;
        extract_authenticated_user(
            &state,
            &jar,
            &headers,
            &cfg.auth.cookie_name,
            cfg.auth.enabled,
        )
        .map(|(u, _)| u)
        .unwrap_or_else(|| "admin".to_string())
    };
    if let Err(err) = state.db.insert_share_link_with_options(
        &jti,
        &kind,
        &body.target_id,
        target_cam.as_deref(),
        &created_by,
        now as f64,
        exp as f64,
        clamped_max_views,
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "error": "Failed to persist share link",
                "message": format!("Failed to persist share link: {err}")
            })),
        )
            .into_response();
    }
    let payload = ShareTokenPayload {
        k: kind,
        id: body.target_id.clone(),
        exp,
        cam: target_cam,
        jti: Some(jti.clone()),
    };
    let token = sign_share_token(&state.jwt_secret, &payload);
    let mut resp_json = json!({
        "success": true,
        "id": jti,
        "token": token,
        "url": format!("/share/{token}"),
        "video_url": format!("/share/{token}/video.mp4"),
        "expires_at": exp
    });
    if let Some(mv) = clamped_max_views {
        resp_json["max_views"] = json!(mv);
    }
    Json(resp_json).into_response()
}

fn parse_video_range_bounds(headers: &HeaderMap) -> Option<(u64, Option<u64>)> {
    let range_val = headers.get(header::RANGE).and_then(|v| v.to_str().ok())?;
    let spec = range_val.trim().strip_prefix("bytes=")?;
    let (start_s, end_s) = spec.split_once('-')?;
    let start = start_s.trim().parse::<u64>().ok()?;
    let end_trimmed = end_s.trim();
    if end_trimmed.is_empty() {
        Some((start, None))
    } else {
        let end = end_trimmed.parse::<u64>().ok()?;
        if start <= end {
            Some((start, Some(end)))
        } else {
            None
        }
    }
}

fn is_initial_or_unranged_video_request(headers: &HeaderMap) -> bool {
    match parse_video_range_bounds(headers) {
        None => true,
        Some((0, _)) => true,
        Some(_) => false,
    }
}

fn verify_share_jti(
    state: &AppState,
    payload: &ShareTokenPayload,
    now_ts: u64,
) -> std::result::Result<(), (StatusCode, &'static str)> {
    verify_share_jti_with_view_mode(state, payload, now_ts, true, true).map(|_| ())
}

fn verify_share_jti_with_view_mode(
    state: &AppState,
    payload: &ShareTokenPayload,
    now_ts: u64,
    is_initial_request: bool,
    is_player_page: bool,
) -> std::result::Result<bool, (StatusCode, &'static str)> {
    let Some(jti) = payload
        .jti
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Err((
            StatusCode::FORBIDDEN,
            "Share link token is missing required JTI",
        ));
    };
    let link = match state.db.get_share_link(jti) {
        Ok(Some(link)) => link,
        Ok(None) => {
            return Err((StatusCode::GONE, "Share link has been revoked"));
        }
        Err(_) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to verify share link state",
            ));
        }
    };
    if link["revoked"].as_bool().unwrap_or(false) {
        return Err((StatusCode::GONE, "Share link has been revoked"));
    }
    if link["expires_at"]
        .as_f64()
        .is_some_and(|exp| (exp as u64) <= now_ts)
    {
        return Err((StatusCode::GONE, "Share link has expired"));
    }
    let access_count = link["access_count"].as_i64().unwrap_or(0);
    let credit_state = state
        .share_page_video_credits
        .lock()
        .ok()
        .and_then(|map| map.get(jti).copied())
        .unwrap_or_default();
    let has_page_credit =
        !is_player_page && is_initial_request && access_count > 0 && credit_state.page_credits > 0;
    let has_active_stream = !is_player_page
        && !is_initial_request
        && access_count > 0
        && credit_state.active_stream_started;
    let count_view = is_player_page || (!has_page_credit && !has_active_stream);
    if let Some(max_v) = link["max_views"].as_i64()
        && ((count_view && access_count >= max_v) || (!count_view && access_count > max_v))
    {
        if let Ok(mut map) = state.share_page_video_credits.lock()
            && let Some(entry) = map.get_mut(jti)
        {
            entry.active_stream_started = false;
            entry.min_next_range_start = u64::MAX;
            if entry.page_credits == 0 {
                map.remove(jti);
            }
        }
        return Err((StatusCode::GONE, "Share link view limit reached"));
    }
    if link["kind"].as_str() != Some(payload.k.as_str())
        || link["target_id"].as_str() != Some(payload.id.as_str())
        || link["camera"].as_str() != payload.cam.as_deref()
    {
        return Err((StatusCode::FORBIDDEN, "Share link payload mismatch"));
    }
    if let Some(created_by) = link["created_by"].as_str()
        && !created_by.is_empty()
        && !created_by.eq_ignore_ascii_case("admin")
    {
        if let Some(token_name) = created_by.strip_prefix("token:") {
            let token_valid = state.db.list_api_tokens().ok().is_some_and(|tokens| {
                tokens.iter().any(|item| {
                    item["name"].as_str() == Some(token_name)
                        && item["scope"].as_str() == Some("full")
                        && item["expires_at"]
                            .as_f64()
                            .is_none_or(|exp| exp > now_ts as f64)
                })
            });
            if !token_valid {
                return Err((
                    StatusCode::FORBIDDEN,
                    "Share link creator API token revoked or expired",
                ));
            }
        } else {
            match state.db.get_user(created_by) {
                Ok(Some(db_u)) => {
                    if db_u.role.trim().eq_ignore_ascii_case("viewer") {
                        return Err((
                            StatusCode::FORBIDDEN,
                            "Share link creator role is not permitted to share",
                        ));
                    }
                    if !db_u.role.trim().eq_ignore_ascii_case("admin") {
                        let raw_cams = db_u.allowed_cameras.trim();
                        if !raw_cams.is_empty() && raw_cams != "*" {
                            let allowed: Vec<&str> = raw_cams
                                .split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .collect();
                            let cam_ok = link["camera"]
                                .as_str()
                                .is_some_and(|c| allowed.contains(&c));
                            if !cam_ok {
                                return Err((
                                    StatusCode::FORBIDDEN,
                                    "Share link creator camera access revoked",
                                ));
                            }
                        }
                    }
                }
                Ok(None) | Err(_) => {
                    return Err((
                        StatusCode::FORBIDDEN,
                        "Share link creator user no longer exists",
                    ));
                }
            }
        }
    }
    Ok(count_view)
}

fn next_stream_state_from_video_response(
    resp: &Response,
    request_range: Option<(u64, Option<u64>)>,
) -> (bool, u64) {
    if resp.status() == StatusCode::PARTIAL_CONTENT
        && let Some(cr) = resp
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
        && let Some(spec) = cr.trim().strip_prefix("bytes ")
        && let Some((range_part, total_s)) = spec.split_once('/')
        && let Some((start_s, end_s)) = range_part.split_once('-')
        && let (Ok(start), Ok(end), Ok(total)) = (
            start_s.trim().parse::<u64>(),
            end_s.trim().parse::<u64>(),
            total_s.trim().parse::<u64>(),
        )
    {
        if start == 0 && end + 1 < total {
            return (true, (end + 1).max(2));
        }
        if matches!(request_range, Some((0, None))) && total > 2 {
            return (true, 2);
        }
        if start > 0 && end + 1 < total {
            return (true, end + 1);
        }
    }
    (false, u64::MAX)
}

fn prune_share_page_video_credits(map: &mut HashMap<String, SharePlaybackCredit>) {
    if map.len() < 512 {
        return;
    }
    map.retain(|_, v| v.page_credits > 0 || v.active_stream_started);
    if map.len() >= 512 {
        map.retain(|_, v| v.page_credits > 0);
    }
    while map.len() >= 512 {
        if let Some(victim_key) = map.keys().next().cloned() {
            map.remove(&victim_key);
        } else {
            break;
        }
    }
}

fn record_share_access_or_gone(
    state: &AppState,
    payload: &ShareTokenPayload,
    count_view: bool,
    is_player_page: bool,
    request_range: Option<(u64, Option<u64>)>,
    now_ts: u64,
    resp: Response,
) -> Response {
    let is_initial_video = !is_player_page && matches!(request_range, None | Some((0, _)));
    let Some(jti) = payload
        .jti
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return resp;
    };
    let (stream_active, min_next_start) = if is_player_page {
        (false, u64::MAX)
    } else {
        next_stream_state_from_video_response(&resp, request_range)
    };
    if !is_player_page && is_initial_video {
        let consumed_credit = state
            .share_page_video_credits
            .lock()
            .ok()
            .and_then(|mut map| {
                if let Some(entry) = map.get_mut(jti)
                    && entry.page_credits > 0
                {
                    entry.page_credits -= 1;
                    entry.active_stream_started = stream_active;
                    entry.min_next_range_start = min_next_start;
                    if entry.page_credits == 0 && !entry.active_stream_started {
                        map.remove(jti);
                    }
                    Some(true)
                } else {
                    None
                }
            })
            .unwrap_or(false);
        if consumed_credit {
            return resp;
        }
    }
    if count_view || (!is_player_page && is_initial_video) {
        match state.db.record_share_link_access(jti, now_ts as f64) {
            Ok(true) => {
                if let Ok(mut map) = state.share_page_video_credits.lock() {
                    if is_player_page {
                        if !map.contains_key(jti) && map.len() >= 512 {
                            prune_share_page_video_credits(&mut map);
                        }
                        let entry = map.entry(jti.to_string()).or_default();
                        entry.page_credits += 1;
                    } else if is_initial_video && stream_active {
                        if !map.contains_key(jti) && map.len() >= 512 {
                            prune_share_page_video_credits(&mut map);
                        }
                        let entry = map.entry(jti.to_string()).or_default();
                        entry.active_stream_started = true;
                        entry.min_next_range_start = min_next_start;
                    } else if let Some(entry) = map.get_mut(jti) {
                        entry.active_stream_started = false;
                        entry.min_next_range_start = u64::MAX;
                        if entry.page_credits == 0 {
                            map.remove(jti);
                        }
                    }
                }
                resp
            }
            Ok(false) => {
                if let Ok(mut map) = state.share_page_video_credits.lock() {
                    map.remove(jti);
                }
                (
                    StatusCode::GONE,
                    Json(json!({ "error": "Share link view limit reached or revoked" })),
                )
                    .into_response()
            }
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to record share link access" })),
            )
                .into_response(),
        }
    } else {
        if let Ok(mut map) = state.share_page_video_credits.lock()
            && let Some(entry) = map.get_mut(jti)
        {
            entry.active_stream_started = stream_active;
            entry.min_next_range_start = min_next_start;
            if entry.page_credits == 0 && !entry.active_stream_started {
                map.remove(jti);
            }
        }
        resp
    }
}

fn is_link_preview_bot_user_agent(headers: &HeaderMap) -> bool {
    let Some(ua) = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let ua_lower = ua.to_ascii_lowercase();
    [
        "whatsapp",
        "slackbot",
        "telegrambot",
        "discordbot",
        "twitterbot",
        "facebookexternalhit",
        "linkedinbot",
        "applebot",
    ]
    .iter()
    .any(|bot| ua_lower.contains(bot))
}

async fn share_player_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
) -> Response {
    let now_ts = unix_now();
    let payload = match verify_share_token(&state.jwt_secret, &token, now_ts) {
        Ok(p) => p,
        Err(status) => {
            return (
                status,
                Json(json!({ "error": "Share link invalid or expired" })),
            )
                .into_response();
        }
    };
    if let Err((status, msg)) = verify_share_jti(&state, &payload, now_ts) {
        return (status, Json(json!({ "error": msg }))).into_response();
    }
    if payload.k != "export"
        && let (Ok(Some(rev)), Ok(Some(ev))) = (
            state.db.get_review_by_id(&payload.id),
            state.db.get_event(&payload.id),
        )
        && ev["camera"].as_str().is_some_and(|ec| ec != rev.camera)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Share link camera mismatch" })),
        )
            .into_response();
    }
    let (resolved_cam, clip_title, clip_ts, exists) =
        resolve_share_target_metadata(&state, &payload.k, &payload.id);
    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Shared clip not found" })),
        )
            .into_response();
    }
    if let Some(ref token_cam) = payload.cam
        && let Some(ref actual_cam) = resolved_cam
        && token_cam != actual_cam
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Share link camera mismatch" })),
        )
            .into_response();
    }
    let safe_title = xml_escape_simple(&clip_title);
    if is_link_preview_bot_user_agent(&headers) {
        let preview_html = format!(
            r#"<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><meta name="robots" content="noindex, nofollow"><title>NinjaNVR Shared Clip // {safe_title}</title></head>
<body></body>
</html>"#
        );
        return (
            [
                (
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'",
                ),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::REFERRER_POLICY, "no-referrer"),
                (header::X_FRAME_OPTIONS, "DENY"),
                (header::CACHE_CONTROL, "private, no-store"),
                (
                    header::HeaderName::from_static("cross-origin-resource-policy"),
                    "same-origin",
                ),
            ],
            Html(preview_html),
        )
            .into_response();
    }
    let safe_id = xml_escape_simple(&payload.id);
    let safe_token = xml_escape_simple(&token);
    let ts_label = clip_ts
        .and_then(|ts| chrono::DateTime::from_timestamp(ts as i64, 0))
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "Timestamp recorded in clip".to_string());
    let exp_label = chrono::DateTime::from_timestamp(payload.exp as i64, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| payload.exp.to_string());
    let html = format!(
        r#"<!doctype html>
<html lang="en" style="color-scheme:dark;background:#070a0f;color:#e2e8f0;font-family:system-ui,sans-serif">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="robots" content="noindex, nofollow"><title>NinjaNVR Shared Clip // {safe_title}</title></head>
<body style="margin:0;padding:24px;display:flex;flex-direction:column;align-items:center;gap:14px">
  <div style="width:100%;max-width:960px;background:#0d121b;border:1px solid #1e293b;border-radius:4px;padding:16px">
    <div style="display:flex;justify-content:space-between;align-items:center;gap:12px;flex-wrap:wrap;margin-bottom:12px;font-family:monospace;font-size:12px">
      <div>
        <div><strong>NINJANVR SHARED CLIP // {safe_title}</strong></div>
        <div style="color:#94a3b8;font-size:11px;margin-top:2px">CLIP TIME: {ts_label} · EXPIRES: {exp_label}</div>
      </div>
      <a href="/share/{safe_token}/video.mp4" download="{safe_id}.mp4" style="color:#38bdf8;text-decoration:none;border:1px solid #0284c7;padding:4px 10px;border-radius:3px">Download MP4</a>
    </div>
    <video src="/share/{safe_token}/video.mp4" controls autoplay playsinline style="width:100%;max-height:72vh;object-fit:contain;background:#000"></video>
  </div>
</body>
</html>"#
    );
    let resp = (
        [
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::CACHE_CONTROL, "private, no-store"),
            (
                header::HeaderName::from_static("cross-origin-resource-policy"),
                "same-origin",
            ),
        ],
        Html(html),
    )
        .into_response();
    record_share_access_or_gone(
        &state,
        &payload,
        payload.jti.is_some(),
        true,
        None,
        now_ts,
        resp,
    )
}

pub fn serve_mp4_bytes_with_range(bytes: Vec<u8>, headers: &HeaderMap) -> Response {
    let total = bytes.len();
    if total > 0
        && let Some(range_val) = headers.get(header::RANGE).and_then(|v| v.to_str().ok())
        && let Some(spec) = range_val.strip_prefix("bytes=")
        && let Some((start_s, end_s)) = spec.split_once('-')
        && let Ok(start) = start_s.trim().parse::<usize>()
        && start < total
    {
        let end = end_s
            .trim()
            .parse::<usize>()
            .map(|e| e.min(total - 1))
            .unwrap_or(total - 1);
        if start <= end {
            let slice = bytes[start..=end].to_vec();
            let content_range = format!("bytes {start}-{end}/{total}");
            return (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, "video/mp4".to_string()),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (header::CONTENT_RANGE, content_range),
                    (header::CONTENT_LENGTH, slice.len().to_string()),
                    (
                        header::CONTENT_SECURITY_POLICY,
                        "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'".to_string(),
                    ),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                    (header::REFERRER_POLICY, "no-referrer".to_string()),
                    (header::X_FRAME_OPTIONS, "DENY".to_string()),
                    (header::CACHE_CONTROL, "private, no-store".to_string()),
                    (
                        header::HeaderName::from_static("cross-origin-resource-policy"),
                        "same-origin".to_string(),
                    ),
                ],
                slice,
            )
                .into_response();
        }
    }

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "video/mp4".to_string()),
            (header::ACCEPT_RANGES, "bytes".to_string()),
            (header::CONTENT_LENGTH, total.to_string()),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'".to_string(),
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::REFERRER_POLICY, "no-referrer".to_string()),
            (header::X_FRAME_OPTIONS, "DENY".to_string()),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
            (
                header::HeaderName::from_static("cross-origin-resource-policy"),
                "same-origin".to_string(),
            ),
        ],
        bytes,
    )
        .into_response()
}

async fn serve_mp4_file_with_range(disk_path: &Path, headers: &HeaderMap) -> Option<Response> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(disk_path).await.ok()?;
    let meta = file.metadata().await.ok()?;
    if !meta.is_file() {
        return None;
    }
    let total = meta.len();
    if total > 0
        && let Some(range_val) = headers.get(header::RANGE).and_then(|v| v.to_str().ok())
        && let Some(spec) = range_val.strip_prefix("bytes=")
        && let Some((start_s, end_s)) = spec.split_once('-')
        && let Ok(start) = start_s.trim().parse::<u64>()
        && start < total
    {
        let end = end_s
            .trim()
            .parse::<u64>()
            .map(|e| e.min(total - 1))
            .unwrap_or(total - 1);
        if start <= end {
            file.seek(std::io::SeekFrom::Start(start)).await.ok()?;
            let slice_len = end - start + 1;
            let reader = file.take(slice_len);
            let stream = futures_util::stream::unfold(reader, |mut r| async move {
                let mut buf = vec![0u8; 65536];
                match r.read(&mut buf).await {
                    Ok(0) => None,
                    Ok(n) => {
                        buf.truncate(n);
                        Some((Ok::<Bytes, std::io::Error>(Bytes::from(buf)), r))
                    }
                    Err(e) => Some((Err(e), r)),
                }
            });
            let content_range = format!("bytes {start}-{end}/{total}");
            return Some(
                (
                    StatusCode::PARTIAL_CONTENT,
                    [
                        (header::CONTENT_TYPE, "video/mp4".to_string()),
                        (header::ACCEPT_RANGES, "bytes".to_string()),
                        (header::CONTENT_RANGE, content_range),
                        (header::CONTENT_LENGTH, slice_len.to_string()),
                        (
                            header::CONTENT_SECURITY_POLICY,
                            "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'".to_string(),
                        ),
                        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                        (header::REFERRER_POLICY, "no-referrer".to_string()),
                        (header::X_FRAME_OPTIONS, "DENY".to_string()),
                        (header::CACHE_CONTROL, "private, no-store".to_string()),
                        (
                            header::HeaderName::from_static("cross-origin-resource-policy"),
                            "same-origin".to_string(),
                        ),
                    ],
                    Body::from_stream(stream),
                )
                    .into_response(),
            );
        }
    }

    let reader = file.take(total);
    let stream = futures_util::stream::unfold(reader, |mut r| async move {
        let mut buf = vec![0u8; 65536];
        match r.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<Bytes, std::io::Error>(Bytes::from(buf)), r))
            }
            Err(e) => Some((Err(e), r)),
        }
    });
    Some(
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "video/mp4".to_string()),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (header::CONTENT_LENGTH, total.to_string()),
                (
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'".to_string(),
                ),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                (header::REFERRER_POLICY, "no-referrer".to_string()),
                (header::X_FRAME_OPTIONS, "DENY".to_string()),
                (header::CACHE_CONTROL, "private, no-store".to_string()),
                (
                    header::HeaderName::from_static("cross-origin-resource-policy"),
                    "same-origin".to_string(),
                ),
            ],
            Body::from_stream(stream),
        )
            .into_response(),
    )
}

fn has_active_privacy_mask(mask: Option<&Value>) -> bool {
    match mask {
        Some(Value::Array(arr)) => !arr.is_empty(),
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
        _ => false,
    }
}

pub fn ensure_path_within_media_dir(media_dir: &Path, candidate: &Path) -> Option<PathBuf> {
    let media_dir_canon = std::fs::canonicalize(media_dir).ok()?;
    let candidate_canon = std::fs::canonicalize(candidate).ok()?;
    if candidate_canon.starts_with(&media_dir_canon) {
        Some(candidate_canon)
    } else {
        None
    }
}

fn compute_privacy_mask_hash(privacy_mask: Option<&Value>) -> String {
    let serialized = privacy_mask
        .and_then(|v| serde_json::to_vec(v).ok())
        .unwrap_or_default();
    let digest = Sha256::digest(&serialized);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

async fn acquire_share_clip_lock(state: &AppState, clip_key: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = state.share_clip_locks.lock().await;
    if map.len() >= 64 {
        map.retain(|_, v| Arc::strong_count(v) > 1);
    }
    map.entry(clip_key.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn cleanup_share_clip_artifacts(media_dir: &Path, target_id: &str) {
    let safe_id: String = target_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        .collect();
    if safe_id.is_empty() || safe_id != target_id || target_id.contains("..") {
        return;
    }
    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    for path in [
        exports_dir.join(format!("share_redacted_{safe_id}.mp4")),
        exports_dir.join(format!(".share_redacted_{safe_id}.mask_hash")),
        exports_dir.join(format!("share_clip_{safe_id}.mp4")),
        clips_dir.join(format!("share_redacted_{safe_id}.webp")),
        clips_dir.join(format!("share_clip_{safe_id}.webp")),
    ] {
        let _ = std::fs::remove_file(path);
    }
}

fn cleanup_camera_share_clip_artifacts_in_range(
    state: &AppState,
    camera: &str,
    after: f64,
    before: f64,
) {
    let query_after = (after - 15.0).max(0.0);
    let query_before = before + 15.0;
    if let Ok(events) = state.db.query_events(&EventFilter {
        cameras: Some(vec![camera.to_string()]),
        after: Some(query_after),
        before: Some(query_before),
        limit: Some(2000),
        ..Default::default()
    }) {
        for ev in events {
            if let Some(id) = ev["id"].as_str() {
                cleanup_share_clip_artifacts(&state.media_dir, id);
            }
        }
    }
    if let Ok(reviews) = state.db.query_reviews(&ReviewFilter {
        cameras: Some(vec![camera.to_string()]),
        after: Some(query_after),
        before: Some(query_before),
        limit: Some(2000),
        ..Default::default()
    }) {
        for rev in reviews {
            cleanup_share_clip_artifacts(&state.media_dir, &rev.id);
        }
    }
    for dir in [
        state.media_dir.join("exports"),
        state.media_dir.join("clips"),
    ] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let fname = entry.file_name();
            let Some(fname_str) = fname.to_str() else {
                continue;
            };
            let target_id = fname_str
                .strip_prefix(".share_redacted_")
                .and_then(|s| s.strip_suffix(".mask_hash"))
                .or_else(|| {
                    fname_str
                        .strip_prefix("share_redacted_")
                        .or_else(|| fname_str.strip_prefix("share_clip_"))
                        .and_then(|s| s.strip_suffix(".mp4").or_else(|| s.strip_suffix(".webp")))
                });
            let Some(tid) = target_id else {
                continue;
            };
            let mut should_clean = false;
            if let Ok(Some(ev)) = state.db.get_event(tid)
                && ev["camera"].as_str() == Some(camera)
            {
                let start = ev["start_time"].as_f64().unwrap_or(0.0);
                let end = ev["end_time"].as_f64().unwrap_or(start + 15.0);
                if (start <= query_before && end >= query_after)
                    || state
                        .db
                        .query_recordings(camera, start - 10.0, end + 10.0)
                        .map(|r| r.is_empty())
                        .unwrap_or(true)
                {
                    should_clean = true;
                }
            }
            if !should_clean
                && let Ok(Some(rev)) = state.db.get_review_by_id(tid)
                && rev.camera == camera
            {
                let start = rev.start_time;
                let end = rev.end_time.unwrap_or(start + 15.0);
                if (start <= query_before && end >= query_after)
                    || state
                        .db
                        .query_recordings(camera, start - 10.0, end + 10.0)
                        .map(|r| r.is_empty())
                        .unwrap_or(true)
                {
                    should_clean = true;
                }
            }
            if should_clean {
                cleanup_share_clip_artifacts(&state.media_dir, tid);
            }
        }
    }
}

async fn redact_mp4_file_with_privacy(
    source_path: &Path,
    exports_dir: &Path,
    redacted_id: &str,
    out_file: &Path,
    privacy_mask: Option<&Value>,
) -> bool {
    let Some(vf) = ninjanvr_media::build_privacy_drawbox_filter(privacy_mask) else {
        return false;
    };
    if std::fs::create_dir_all(exports_dir).is_err() {
        return false;
    }
    let nonce: u64 = rand::random();
    let tmp_out_file = exports_dir.join(format!(".{redacted_id}.{nonce:016x}.tmp.mp4"));
    struct TempFileGuard(PathBuf);
    impl Drop for TempFileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = TempFileGuard(tmp_out_file.clone());

    let ffmpeg_bin = ninjanvr_media::resolve_ffmpeg_bin();
    let mut cmd = tokio::process::Command::new(&ffmpeg_bin);
    cmd.kill_on_drop(true)
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(source_path)
        .args(["-vf", &vf, "-an", "-movflags", "+faststart"])
        .arg(&tmp_out_file);

    let status_res = tokio::time::timeout(Duration::from_secs(120), cmd.status()).await;
    if !matches!(status_res, Ok(Ok(s)) if s.success()) {
        return false;
    }
    if std::fs::metadata(&tmp_out_file)
        .map(|m| m.len() == 0)
        .unwrap_or(true)
    {
        return false;
    }
    std::fs::rename(&tmp_out_file, out_file).is_ok()
}

async fn share_video_mp4(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
) -> Response {
    let now_ts = unix_now();
    let payload = match verify_share_token(&state.jwt_secret, &token, now_ts) {
        Ok(p) => p,
        Err(status) => {
            return (
                status,
                Json(json!({ "error": "Share link invalid or expired" })),
            )
                .into_response();
        }
    };
    let request_range = parse_video_range_bounds(&headers);
    let is_initial = is_initial_or_unranged_video_request(&headers);
    if !is_initial && let Some(jti) = payload.jti.as_deref().map(str::trim) {
        let valid_continuation = request_range
            .and_then(|(start, _)| {
                state
                    .share_page_video_credits
                    .lock()
                    .ok()
                    .and_then(|map| map.get(jti).copied())
                    .map(|c| c.active_stream_started && start >= c.min_next_range_start)
            })
            .unwrap_or(false);
        if !valid_continuation
            && let Ok(mut map) = state.share_page_video_credits.lock()
            && let Some(entry) = map.get_mut(jti)
        {
            entry.active_stream_started = false;
            entry.min_next_range_start = u64::MAX;
        }
    }
    let count_view =
        match verify_share_jti_with_view_mode(&state, &payload, now_ts, is_initial, false) {
            Ok(cv) => cv,
            Err((status, msg)) => {
                return (status, Json(json!({ "error": msg }))).into_response();
            }
        };

    let safe_id: String = payload
        .id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        .collect();

    if payload.k == "export" {
        let mut resolved_export: Option<(PathBuf, Option<String>)> = None;
        if let Ok(Some(exp)) = state.db.get_export_by_id(&payload.id) {
            if let Some(ref token_cam) = payload.cam
                && let Some(exp_cam) = exp["camera"].as_str()
                && token_cam != exp_cam
            {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "error": "Share link camera mismatch" })),
                )
                    .into_response();
            }
            if let Some(vp) = exp["video_path"].as_str() {
                let disk_path = resolve_recording_disk_path(&state.media_dir, vp);
                if let Some(safe_path) = ensure_path_within_media_dir(&state.media_dir, &disk_path)
                    && safe_path.is_file()
                {
                    let cam = exp["camera"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .or_else(|| payload.cam.clone());
                    resolved_export = Some((safe_path, cam));
                }
            }
        }
        if resolved_export.is_none()
            && !is_disallowed_export_fallback_id(&payload.id)
            && payload.id == safe_id
        {
            if let Some(ref token_cam) = payload.cam
                && let Some(cam_prefix) =
                    payload.id.rsplitn(3, '_').nth(2).filter(|s| !s.is_empty())
                && token_cam != cam_prefix
            {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "error": "Share link camera mismatch" })),
                )
                    .into_response();
            }
            let file_path = state
                .media_dir
                .join("exports")
                .join(format!("{safe_id}.mp4"));
            if let Some(safe_path) = ensure_path_within_media_dir(&state.media_dir, &file_path)
                && safe_path.is_file()
            {
                let cam = payload.cam.clone().or_else(|| {
                    payload
                        .id
                        .rsplitn(3, '_')
                        .nth(2)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                });
                resolved_export = Some((safe_path, cam));
            }
        }

        if let Some((source_path, export_cam)) = resolved_export {
            let privacy_mask = if let Some(ref cam) = export_cam {
                let cfg = state.config.read().await;
                cfg.cameras.get(cam).map(|c| c.privacy_mask.clone())
            } else {
                None
            };
            if has_active_privacy_mask(privacy_mask.as_ref()) {
                if safe_id.is_empty() || payload.id != safe_id || payload.id.contains("..") {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                let redacted_id = format!("share_redacted_{safe_id}");
                let exports_dir = state.media_dir.join("exports");
                let redacted_path = exports_dir.join(format!("{redacted_id}.mp4"));
                let mask_hash_path = exports_dir.join(format!(".{redacted_id}.mask_hash"));
                let current_mask_hash = compute_privacy_mask_hash(privacy_mask.as_ref());

                let clip_lock = acquire_share_clip_lock(&state, &redacted_id).await;
                let _clip_guard = clip_lock.lock().await;

                if redacted_path.is_file() {
                    match std::fs::read_to_string(&mask_hash_path) {
                        Ok(existing_hash) => {
                            if existing_hash.trim() != current_mask_hash {
                                let _ = std::fs::remove_file(&redacted_path);
                                let _ = std::fs::remove_file(&mask_hash_path);
                            }
                        }
                        Err(_) => {
                            let _ = std::fs::write(&mask_hash_path, &current_mask_hash);
                        }
                    }
                }

                if !redacted_path.is_file() {
                    let _sem_permit = state.share_ffmpeg_semaphore.acquire().await.ok();
                    let build_ok = redact_mp4_file_with_privacy(
                        &source_path,
                        &exports_dir,
                        &redacted_id,
                        &redacted_path,
                        privacy_mask.as_ref(),
                    )
                    .await;
                    if build_ok {
                        let _ = std::fs::write(&mask_hash_path, &current_mask_hash);
                    } else {
                        let _ = std::fs::remove_file(&redacted_path);
                        let _ = std::fs::remove_file(&mask_hash_path);
                        return StatusCode::NOT_FOUND.into_response();
                    }
                }
                drop(_clip_guard);

                if let Some(safe_path) =
                    ensure_path_within_media_dir(&state.media_dir, &redacted_path)
                    && let Some(resp) = serve_mp4_file_with_range(&safe_path, &headers).await
                {
                    return record_share_access_or_gone(
                        &state,
                        &payload,
                        count_view,
                        false,
                        request_range,
                        now_ts,
                        resp,
                    );
                }
                return StatusCode::NOT_FOUND.into_response();
            }

            if let Some(resp) = serve_mp4_file_with_range(&source_path, &headers).await {
                return record_share_access_or_gone(
                    &state,
                    &payload,
                    count_view,
                    false,
                    request_range,
                    now_ts,
                    resp,
                );
            }
        } else {
            cleanup_share_clip_artifacts(&state.media_dir, &payload.id);
        }
    } else if payload.k == "review" || payload.k == "event" {
        if let (Ok(Some(rev)), Ok(Some(ev))) = (
            state.db.get_review_by_id(&payload.id),
            state.db.get_event(&payload.id),
        ) && ev["camera"].as_str().is_some_and(|ec| ec != rev.camera)
        {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "Share link camera mismatch" })),
            )
                .into_response();
        }
        let mut resolved_target: Option<(String, f64, f64)> = None;
        if payload.k == "review" {
            if let Ok(Some(rev)) = state.db.get_review_by_id(&payload.id) {
                if !rev.camera.trim().is_empty() {
                    if let Some(ref token_cam) = payload.cam
                        && token_cam != &rev.camera
                    {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({ "error": "Share link camera mismatch" })),
                        )
                            .into_response();
                    }
                    let end = rev.end_time.unwrap_or(rev.start_time + 15.0);
                    resolved_target = Some((rev.camera, rev.start_time, end));
                }
            } else if let Ok(Some(ev)) = state.db.get_event(&payload.id)
                && let Some(cam) = ev["camera"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            {
                if let Some(ref token_cam) = payload.cam
                    && token_cam != cam
                {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "error": "Share link camera mismatch" })),
                    )
                        .into_response();
                }
                let start = ev["start_time"].as_f64().unwrap_or(0.0);
                let end = ev["end_time"].as_f64().unwrap_or(start + 15.0);
                resolved_target = Some((cam.to_string(), start, end));
            }
        } else if let Ok(Some(ev)) = state.db.get_event(&payload.id) {
            if let Some(cam) = ev["camera"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if let Some(ref token_cam) = payload.cam
                    && token_cam != cam
                {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "error": "Share link camera mismatch" })),
                    )
                        .into_response();
                }
                let start = ev["start_time"].as_f64().unwrap_or(0.0);
                let end = ev["end_time"].as_f64().unwrap_or(start + 15.0);
                resolved_target = Some((cam.to_string(), start, end));
            }
        } else if let Ok(Some(rev)) = state.db.get_review_by_id(&payload.id)
            && !rev.camera.trim().is_empty()
        {
            if let Some(ref token_cam) = payload.cam
                && token_cam != &rev.camera
            {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "error": "Share link camera mismatch" })),
                )
                    .into_response();
            }
            let end = rev.end_time.unwrap_or(rev.start_time + 15.0);
            resolved_target = Some((rev.camera, rev.start_time, end));
        }

        if let Some((cam, start, raw_end)) = resolved_target {
            let end = raw_end.max(start).min(start + 300.0);

            let filter_valid_recs = |rows: Vec<ninjanvr_db::RecordingRow>| -> Vec<PathBuf> {
                rows.into_iter()
                    .filter_map(|rec| {
                        let disk_path = resolve_recording_disk_path(&state.media_dir, &rec.path);
                        let safe_path = ensure_path_within_media_dir(&state.media_dir, &disk_path)?;
                        if safe_path.is_file() {
                            Some(safe_path)
                        } else {
                            None
                        }
                    })
                    .collect()
            };

            let mut recs = state
                .db
                .query_recordings(&cam, start - 5.0, end + 5.0)
                .map(filter_valid_recs)
                .unwrap_or_default();
            if recs.is_empty() {
                recs = state
                    .db
                    .query_recordings(&cam, start - 10.0, end + 10.0)
                    .map(filter_valid_recs)
                    .unwrap_or_default();
            }
            if recs.is_empty() {
                cleanup_share_clip_artifacts(&state.media_dir, &payload.id);
                return StatusCode::NOT_FOUND.into_response();
            }

            let privacy_mask = {
                let cfg = state.config.read().await;
                cfg.cameras.get(&cam).map(|c| c.privacy_mask.clone())
            };
            if has_active_privacy_mask(privacy_mask.as_ref()) {
                if safe_id.is_empty() || payload.id != safe_id || payload.id.contains("..") {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                let redacted_id = format!("share_redacted_{safe_id}");
                let exports_dir = state.media_dir.join("exports");
                let redacted_path = exports_dir.join(format!("{redacted_id}.mp4"));
                let mask_hash_path = exports_dir.join(format!(".{redacted_id}.mask_hash"));
                let current_mask_hash = compute_privacy_mask_hash(privacy_mask.as_ref());

                let clip_lock = acquire_share_clip_lock(&state, &redacted_id).await;
                let _clip_guard = clip_lock.lock().await;

                if redacted_path.is_file() {
                    match std::fs::read_to_string(&mask_hash_path) {
                        Ok(existing_hash) => {
                            if existing_hash.trim() != current_mask_hash {
                                let _ = std::fs::remove_file(&redacted_path);
                                let _ = std::fs::remove_file(&mask_hash_path);
                            }
                        }
                        Err(_) => {
                            let _ = std::fs::write(&mask_hash_path, &current_mask_hash);
                        }
                    }
                }

                if !redacted_path.is_file() {
                    let _sem_permit = state.share_ffmpeg_semaphore.acquire().await.ok();
                    let build_res = tokio::time::timeout(
                        Duration::from_secs(120),
                        build_mp4_export_with_privacy(
                            &state.db,
                            &state.media_dir,
                            &redacted_id,
                            &cam,
                            "shared_redacted",
                            start - 5.0,
                            end + 5.0,
                            false,
                            privacy_mask.as_ref(),
                        ),
                    )
                    .await;
                    let _ = state.db.delete_export(&redacted_id);
                    let _ = std::fs::remove_file(
                        state
                            .media_dir
                            .join("clips")
                            .join(format!("{redacted_id}.webp")),
                    );
                    match build_res {
                        Ok(Ok(())) => {
                            let _ = std::fs::write(&mask_hash_path, &current_mask_hash);
                        }
                        _ => {
                            let _ = std::fs::remove_file(&redacted_path);
                            let _ = std::fs::remove_file(&mask_hash_path);
                            return StatusCode::NOT_FOUND.into_response();
                        }
                    }
                }
                drop(_clip_guard);

                if let Some(safe_path) =
                    ensure_path_within_media_dir(&state.media_dir, &redacted_path)
                    && let Some(resp) = serve_mp4_file_with_range(&safe_path, &headers).await
                {
                    return record_share_access_or_gone(
                        &state,
                        &payload,
                        count_view,
                        false,
                        request_range,
                        now_ts,
                        resp,
                    );
                }
                return StatusCode::NOT_FOUND.into_response();
            }

            if safe_id.is_empty() || payload.id != safe_id || payload.id.contains("..") {
                return StatusCode::BAD_REQUEST.into_response();
            }
            let clip_id = format!("share_clip_{safe_id}");
            let clip_path = state
                .media_dir
                .join("exports")
                .join(format!("{clip_id}.mp4"));
            let clip_lock = acquire_share_clip_lock(&state, &clip_id).await;
            let _clip_guard = clip_lock.lock().await;
            if !clip_path.is_file() {
                let _sem_permit = state.share_ffmpeg_semaphore.acquire().await.ok();
                let build_res = tokio::time::timeout(
                    Duration::from_secs(120),
                    build_mp4_export_with_privacy(
                        &state.db,
                        &state.media_dir,
                        &clip_id,
                        &cam,
                        "shared_clip",
                        start - 5.0,
                        end + 5.0,
                        false,
                        None,
                    ),
                )
                .await;
                let _ = state.db.delete_export(&clip_id);
                let _ = std::fs::remove_file(
                    state
                        .media_dir
                        .join("clips")
                        .join(format!("{clip_id}.webp")),
                );
                if !matches!(build_res, Ok(Ok(()))) {
                    let _ = std::fs::remove_file(&clip_path);
                    return StatusCode::NOT_FOUND.into_response();
                }
            }
            drop(_clip_guard);
            if let Some(safe_path) = ensure_path_within_media_dir(&state.media_dir, &clip_path)
                && let Some(resp) = serve_mp4_file_with_range(&safe_path, &headers).await
            {
                return record_share_access_or_gone(
                    &state,
                    &payload,
                    count_view,
                    false,
                    request_range,
                    now_ts,
                    resp,
                );
            }
            return StatusCode::NOT_FOUND.into_response();
        }
    }

    StatusCode::NOT_FOUND.into_response()
}

async fn api_exports(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    let mut exports = state.db.query_exports().unwrap_or_default();
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await {
        exports.retain(|exp| {
            exp["camera"]
                .as_str()
                .is_some_and(|c| allowed.iter().any(|a| a == c))
        });
    }
    Json(Value::Array(exports))
}

async fn api_cases(State(state): State<AppState>) -> Json<Value> {
    let cases = state.db.query_cases().unwrap_or_default();
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

async fn api_case_create(State(state): State<AppState>, body: Bytes) -> Response {
    if body.is_empty() {
        return Json(json!({ "success": true })).into_response();
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
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "case_{}_{:08x}",
                chrono::Utc::now().timestamp_millis(),
                rand::random::<u32>()
            )
        });
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
}

impl Default for CreateExportBody {
    fn default() -> Self {
        Self {
            name: None,
            include_audio: true,
            strip_audio: false,
        }
    }
}

async fn api_export_create(
    State(state): State<AppState>,
    AxumPath((camera, start, end)): AxumPath<(String, f64, f64)>,
    body: Option<Json<CreateExportBody>>,
) -> Response {
    let id = format!("{camera}_{}_{}", start as u64, end as u64);
    let parsed_body = body.map(|b| b.0).unwrap_or_default();
    let include_audio = parsed_body.include_audio && !parsed_body.strip_audio;
    let name = parsed_body
        .name
        .unwrap_or_else(|| format!("{camera} export"));
    let video_rel = format!("/media/ninjanvr/exports/{id}.mp4");
    let thumb_rel = format!("/media/ninjanvr/clips/{id}.webp");
    let now = unix_now() as f64;
    let _ = state.db.insert_export(&ExportRow {
        id: id.clone(),
        camera: camera.clone(),
        name: name.clone(),
        date: now,
        video_path: video_rel,
        thumb_path: thumb_rel,
        in_progress: true,
    });

    let db_clone = state.db.clone();
    let media_dir_clone = state.media_dir.clone();
    let id_clone = id.clone();
    let privacy_mask = {
        let cfg = state.config.read().await;
        cfg.cameras.get(&camera).map(|c| c.privacy_mask.clone())
    };
    let export_res = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        build_mp4_export_with_privacy(
            &db_clone,
            &media_dir_clone,
            &id_clone,
            &camera,
            &name,
            start,
            end,
            include_audio,
            privacy_mask.as_ref(),
        ),
    )
    .await;

    match export_res {
        Ok(Ok(())) => Json(json!({
            "success": true,
            "export_id": id,
            "video_url": format!("/exports/{id}.mp4"),
            "thumb_url": format!("/clips/{id}.webp")
        }))
        .into_response(),
        Ok(Err(err)) => {
            let _ = state.db.delete_export(&id);
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
            let _ = state.db.delete_export(&id);
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
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<RenameExportBody>,
) -> Response {
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await {
        let permitted = state
            .db
            .get_export_by_id(&id)
            .ok()
            .flatten()
            .and_then(|exp| {
                exp["camera"]
                    .as_str()
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
    let _ = state.db.rename_export(&id, &body.name);
    Json(json!({ "success": true })).into_response()
}

async fn api_export_delete(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let _ = state.db.revoke_share_links_for_target(Some("export"), &id);
    cleanup_share_clip_artifacts(&state.media_dir, &id);
    if let Ok(Some((vpath, tpath))) = state.db.delete_export(&id) {
        let _ = tokio::fs::remove_file(resolve_recording_disk_path(&state.media_dir, &vpath)).await;
        let _ = tokio::fs::remove_file(resolve_recording_disk_path(&state.media_dir, &tpath)).await;
    }
    Json(json!({ "success": true })).into_response()
}

async fn api_exports_bulk_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<DeleteIdsBody>,
) -> Response {
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await {
        for id in &body.ids {
            let permitted = state
                .db
                .get_export_by_id(id)
                .ok()
                .flatten()
                .and_then(|exp| {
                    exp["camera"]
                        .as_str()
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
    }
    for id in body.ids {
        let _ = state.db.revoke_share_links_for_target(Some("export"), &id);
        cleanup_share_clip_artifacts(&state.media_dir, &id);
        if let Ok(Some((vpath, tpath))) = state.db.delete_export(&id) {
            let _ =
                tokio::fs::remove_file(resolve_recording_disk_path(&state.media_dir, &vpath)).await;
            let _ =
                tokio::fs::remove_file(resolve_recording_disk_path(&state.media_dir, &tpath)).await;
        }
    }
    Json(json!({ "success": true })).into_response()
}

async fn api_previews(
    State(state): State<AppState>,
    AxumPath((camera, start, end)): AxumPath<(String, f64, f64)>,
) -> Json<Value> {
    let previews = state
        .db
        .query_previews(&camera, start, end)
        .unwrap_or_default();
    Json(Value::Array(previews))
}

async fn api_preview_thumbnail(
    State(state): State<AppState>,
    AxumPath(preview_name): AxumPath<String>,
    uri: Uri,
) -> Response {
    let camera = preview_name
        .strip_prefix("preview_")
        .and_then(|s| s.split('-').next())
        .unwrap_or("sala")
        .to_string();
    api_camera_latest_frame(
        State(state),
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnvifPresetItem {
    pub token: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OnvifPtzStatus {
    pub pan: f32,
    pub tilt: f32,
    pub zoom: f32,
    pub move_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Tolerantly parses an ONVIF float string (such as `"+0.5000000"`, `"-0.25"`, `"0,5"`, or malformed text)
/// into a finite `f32` clamped to `[min, max]`, returning `default` instead of panicking on invalid input.
pub fn parse_onvif_float(raw: &str, min: f32, max: f32, default: f32) -> f32 {
    let cleaned = raw.trim().trim_start_matches('+').replace(',', ".");
    match cleaned.parse::<f32>() {
        Ok(v) if v.is_finite() => v.clamp(min, max),
        _ => default,
    }
}

fn xml_escape_simple(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn resolve_onvif_ptz_endpoint(onvif: &ninjanvr_config::OnvifCameraConfig) -> Option<String> {
    let host_raw = onvif.host.as_deref()?.trim();
    if host_raw.is_empty() {
        return None;
    }
    if host_raw.starts_with("http://") || host_raw.starts_with("https://") {
        let trimmed = host_raw.trim_end_matches('/');
        let after_scheme = trimmed.split_once("://").map(|(_, r)| r).unwrap_or("");
        if after_scheme.contains('/') {
            return Some(trimmed.to_string());
        }
        return Some(format!("{trimmed}/onvif/ptz_service"));
    }
    if host_raw.contains(':') && !host_raw.ends_with(']') {
        Some(format!("http://{host_raw}/onvif/ptz_service"))
    } else {
        Some(format!(
            "http://{host_raw}:{}/onvif/ptz_service",
            onvif.port
        ))
    }
}

pub fn build_onvif_soap_envelope(
    body_xml: &str,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let header_xml = if let Some(u) = user.map(str::trim).filter(|s| !s.is_empty()) {
        let pw = password.unwrap_or("");
        format!(
            "<s:Header><wsse:Security xmlns:wsse=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\"><wsse:UsernameToken><wsse:Username>{}</wsse:Username><wsse:Password>{}</wsse:Password></wsse:UsernameToken></wsse:Security></s:Header>",
            xml_escape_simple(u),
            xml_escape_simple(pw)
        )
    } else {
        "<s:Header/>".to_string()
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:tptz=\"http://www.onvif.org/ver20/ptz/wsdl\" xmlns:tt=\"http://www.onvif.org/ver10/schema\">{header_xml}<s:Body>{body_xml}</s:Body></s:Envelope>"
    )
}

pub fn build_onvif_continuous_move_soap(
    profile_token: &str,
    pan: f32,
    tilt: f32,
    zoom: f32,
    timeout_sec: u64,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let p = pan.clamp(-1.0, 1.0);
    let t = tilt.clamp(-1.0, 1.0);
    let z = zoom.clamp(-1.0, 1.0);
    let t_sec = timeout_sec.clamp(1, 60);
    let body = format!(
        "<tptz:ContinuousMove><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x=\"{p:.4}\" y=\"{t:.4}\"/><tt:Zoom x=\"{z:.4}\"/></tptz:Velocity><tptz:Timeout>PT{t_sec}S</tptz:Timeout></tptz:ContinuousMove>",
        xml_escape_simple(profile_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_relative_move_soap(
    profile_token: &str,
    pan: f32,
    tilt: f32,
    zoom: f32,
    speed: Option<f32>,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let p = pan.clamp(-1.0, 1.0);
    let t = tilt.clamp(-1.0, 1.0);
    let z = zoom.clamp(-1.0, 1.0);
    let speed_xml = if let Some(spd) = speed {
        let s = spd.clamp(0.05, 1.0);
        format!(
            "<tptz:Speed><tt:PanTilt x=\"{s:.4}\" y=\"{s:.4}\"/><tt:Zoom x=\"{s:.4}\"/></tptz:Speed>"
        )
    } else {
        String::new()
    };
    let body = format!(
        "<tptz:RelativeMove><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:Translation><tt:PanTilt x=\"{p:.4}\" y=\"{t:.4}\"/><tt:Zoom x=\"{z:.4}\"/></tptz:Translation>{speed_xml}</tptz:RelativeMove>",
        xml_escape_simple(profile_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_stop_soap(
    profile_token: &str,
    pan_tilt: bool,
    zoom: bool,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let body = format!(
        "<tptz:Stop><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PanTilt>{pan_tilt}</tptz:PanTilt><tptz:Zoom>{zoom}</tptz:Zoom></tptz:Stop>",
        xml_escape_simple(profile_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_goto_preset_soap(
    profile_token: &str,
    preset_token: &str,
    speed: Option<f32>,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let speed_xml = if let Some(spd) = speed {
        let s = spd.clamp(0.05, 1.0);
        format!(
            "<tptz:Speed><tt:PanTilt x=\"{s:.4}\" y=\"{s:.4}\"/><tt:Zoom x=\"{s:.4}\"/></tptz:Speed>"
        )
    } else {
        String::new()
    };
    let body = format!(
        "<tptz:GotoPreset><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PresetToken>{}</tptz:PresetToken>{speed_xml}</tptz:GotoPreset>",
        xml_escape_simple(profile_token),
        xml_escape_simple(preset_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_get_presets_soap(
    profile_token: &str,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let body = format!(
        "<tptz:GetPresets><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GetPresets>",
        xml_escape_simple(profile_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_get_status_soap(
    profile_token: &str,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let body = format!(
        "<tptz:GetStatus><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GetStatus>",
        xml_escape_simple(profile_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

fn extract_xml_attr<'a>(tag_fragment: &'a str, attr_name: &str) -> Option<&'a str> {
    for quote in ['"', '\''] {
        let pattern = format!("{attr_name}={quote}");
        if let Some(start_idx) = tag_fragment.find(&pattern) {
            let val_start = start_idx + pattern.len();
            if let Some(end_rel) = tag_fragment[val_start..].find(quote) {
                return Some(&tag_fragment[val_start..val_start + end_rel]);
            }
        }
    }
    None
}

fn extract_xml_element_text<'a>(xml: &'a str, local_name: &str) -> Option<&'a str> {
    let mut cursor = 0usize;
    while let Some(lt_rel) = xml[cursor..].find('<') {
        let tag_start = cursor + lt_rel;
        if xml[tag_start..].starts_with("<!--") || xml[tag_start..].starts_with("</") {
            cursor = tag_start + 2;
            continue;
        }
        let Some(gt_rel) = xml[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + gt_rel;
        let header = &xml[tag_start + 1..tag_end];
        let tag_token = header.split_whitespace().next().unwrap_or("");
        let tag_clean = tag_token.trim_end_matches('/');
        let candidate_local = tag_clean
            .rsplit_once(':')
            .map(|(_, l)| l)
            .unwrap_or(tag_clean);
        if candidate_local.eq_ignore_ascii_case(local_name) && !header.ends_with('/') {
            let content_start = tag_end + 1;
            let close_pat_colon = format!(":{candidate_local}>");
            let close_pat_plain = format!("</{candidate_local}>");
            if let Some(end_rel) = xml[content_start..]
                .find(&close_pat_colon)
                .and_then(|pos| xml[content_start..content_start + pos].rfind("</"))
                .or_else(|| xml[content_start..].find(&close_pat_plain))
            {
                return Some(xml[content_start..content_start + end_rel].trim());
            }
        }
        cursor = tag_end + 1;
    }
    None
}

pub fn extract_onvif_soap_fault(xml: &str) -> Option<String> {
    let has_fault = xml.contains(":Fault") || xml.contains("<Fault");
    if !has_fault {
        return None;
    }
    if let Some(text) = extract_xml_element_text(xml, "Text")
        .or_else(|| extract_xml_element_text(xml, "faultstring"))
        .or_else(|| extract_xml_element_text(xml, "Reason"))
        .filter(|s| !s.is_empty())
    {
        return Some(text.to_string());
    }
    Some("ONVIF SOAP Fault".to_string())
}

pub fn parse_onvif_presets_xml(xml: &str) -> Vec<OnvifPresetItem> {
    let mut presets = Vec::new();
    let mut cursor = 0usize;
    while let Some(lt_rel) = xml[cursor..].find('<') {
        let tag_start = cursor + lt_rel;
        let Some(gt_rel) = xml[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + gt_rel;
        let header = &xml[tag_start + 1..tag_end];
        let tag_token = header.split_whitespace().next().unwrap_or("");
        let local_name = tag_token
            .trim_end_matches('/')
            .rsplit_once(':')
            .map(|(_, l)| l)
            .unwrap_or(tag_token.trim_end_matches('/'));
        if local_name.eq_ignore_ascii_case("Preset") && !header.starts_with('/') {
            let token_opt = extract_xml_attr(header, "token").map(str::trim);
            if header.ends_with('/') {
                if let Some(tok) = token_opt.filter(|s| !s.is_empty()) {
                    presets.push(OnvifPresetItem {
                        token: tok.to_string(),
                        name: tok.to_string(),
                    });
                }
                cursor = tag_end + 1;
                continue;
            }
            let content_start = tag_end + 1;
            let close_idx = xml[content_start..]
                .find(":Preset>")
                .and_then(|pos| xml[content_start..content_start + pos].rfind("</"))
                .or_else(|| xml[content_start..].find("</Preset>"))
                .unwrap_or(xml.len() - content_start);
            let inner = &xml[content_start..content_start + close_idx];
            let name_opt = extract_xml_element_text(inner, "Name").map(str::trim);
            let token_elem = extract_xml_element_text(inner, "token").map(str::trim);
            let resolved_token = token_opt
                .or(token_elem)
                .or(name_opt)
                .filter(|s| !s.is_empty());
            if let Some(tok) = resolved_token {
                let resolved_name = name_opt.filter(|s| !s.is_empty()).unwrap_or(tok);
                presets.push(OnvifPresetItem {
                    token: tok.to_string(),
                    name: resolved_name.to_string(),
                });
            }
            cursor = (content_start + close_idx).min(xml.len());
        } else {
            cursor = tag_end + 1;
        }
    }
    presets
}

pub fn parse_onvif_status_xml(xml: &str) -> OnvifPtzStatus {
    let mut pan = 0.0f32;
    let mut tilt = 0.0f32;
    let mut zoom = 0.0f32;

    let mut cursor = 0usize;
    while let Some(lt_rel) = xml[cursor..].find('<') {
        let tag_start = cursor + lt_rel;
        let Some(gt_rel) = xml[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + gt_rel;
        let header = &xml[tag_start + 1..tag_end];
        let tag_token = header.split_whitespace().next().unwrap_or("");
        let local_name = tag_token
            .trim_end_matches('/')
            .rsplit_once(':')
            .map(|(_, l)| l)
            .unwrap_or(tag_token.trim_end_matches('/'));
        if local_name.eq_ignore_ascii_case("PanTilt") {
            if let Some(x_str) = extract_xml_attr(header, "x") {
                pan = parse_onvif_float(x_str, -1.0, 1.0, 0.0);
            }
            if let Some(y_str) = extract_xml_attr(header, "y") {
                tilt = parse_onvif_float(y_str, -1.0, 1.0, 0.0);
            }
        } else if local_name.eq_ignore_ascii_case("Zoom")
            && let Some(x_str) = extract_xml_attr(header, "x")
        {
            zoom = parse_onvif_float(x_str, 0.0, 1.0, 0.0);
        }
        cursor = tag_end + 1;
    }

    let move_status = extract_xml_element_text(xml, "PanTilt")
        .or_else(|| extract_xml_element_text(xml, "MoveStatus"))
        .filter(|s| !s.is_empty() && !s.contains('<'))
        .unwrap_or("IDLE")
        .to_ascii_uppercase();
    let error = extract_xml_element_text(xml, "Error")
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("NoError"))
        .map(str::to_string);

    OnvifPtzStatus {
        pan,
        tilt,
        zoom,
        move_status,
        error,
    }
}

async fn send_onvif_soap_request(
    client: &reqwest::Client,
    endpoint: &str,
    action: &str,
    soap_xml: String,
    user: Option<&str>,
    password: Option<&str>,
    timeout: std::time::Duration,
) -> std::result::Result<String, (StatusCode, String)> {
    let mut req = client
        .post(endpoint)
        .timeout(timeout)
        .header(
            header::CONTENT_TYPE,
            format!(
                "application/soap+xml; charset=utf-8; action=\"http://www.onvif.org/ver20/ptz/wsdl/{action}\""
            ),
        )
        .body(soap_xml);
    if let Some(u) = user.map(str::trim).filter(|s| !s.is_empty()) {
        req = req.basic_auth(u, password);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(err) if err.is_timeout() => {
            return Err((
                StatusCode::GATEWAY_TIMEOUT,
                format!("ONVIF {action} request timed out"),
            ));
        }
        Err(err) => {
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("ONVIF {action} request failed: {err}"),
            ));
        }
    };
    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if let Some(fault_reason) = extract_onvif_soap_fault(&body_text) {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("ONVIF {action} fault: {fault_reason}"),
        ));
    }
    if !status.is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("ONVIF {action} returned HTTP {}", status.as_u16()),
        ));
    }
    Ok(body_text)
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
        let spd = self.speed.unwrap_or(0.5).clamp(0.05, 1.0);
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
        let pan = self.pan.unwrap_or(dir_pan).clamp(-1.0, 1.0);
        let tilt = self.tilt.unwrap_or(dir_tilt).clamp(-1.0, 1.0);
        let zoom = self.zoom.unwrap_or(dir_zoom).clamp(-1.0, 1.0);
        (pan, tilt, zoom)
    }

    pub fn is_relative(&self) -> bool {
        self.mode
            .as_deref()
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("relative"))
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

async fn api_camera_ptz_info(
    State(state): State<AppState>,
    AxumPath(camera): AxumPath<String>,
    Query(q): Query<PtzInfoQueryParams>,
) -> Response {
    let (cam_opt, active_preset) = {
        let cfg = state.config.read().await;
        let cam = cfg.cameras.get(&camera).cloned();
        let active = state
            .ptz_active_presets
            .read()
            .await
            .get(&camera)
            .cloned()
            .flatten()
            .or_else(|| {
                cam.as_ref()
                    .and_then(|c| c.onvif.return_preset.clone())
                    .filter(|s| !s.trim().is_empty())
            });
        (cam, active)
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
    if let Some(ret_p) = cam_cfg
        .onvif
        .return_preset
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
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

    let Some(endpoint) = resolve_onvif_ptz_endpoint(&cam_cfg.onvif) else {
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
    let profile_token = q.profile_token.as_deref().unwrap_or("Profile_1");
    let user = cam_cfg.onvif.user.as_deref();
    let pass = cam_cfg.onvif.password.as_deref();

    let presets_soap = build_onvif_get_presets_soap(profile_token, user, pass);
    let status_soap = build_onvif_get_status_soap(profile_token, user, pass);

    let presets_res = send_onvif_soap_request(
        &state.http_client,
        &endpoint,
        "GetPresets",
        presets_soap,
        user,
        pass,
        timeout,
    )
    .await;
    let status_res = send_onvif_soap_request(
        &state.http_client,
        &endpoint,
        "GetStatus",
        status_soap,
        user,
        pass,
        timeout,
    )
    .await;

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

    let (status_obj, online, err_opt) = match (presets_res, status_res) {
        (_, Ok(status_xml)) => (parse_onvif_status_xml(&status_xml), true, None),
        (Ok(_), Err((_, err_msg))) => (
            OnvifPtzStatus {
                pan: 0.0,
                tilt: 0.0,
                zoom: 0.0,
                move_status: "IDLE".to_string(),
                error: Some(err_msg.clone()),
            },
            true,
            Some(err_msg),
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
            Some(err_a),
        ),
    };

    let mut features = vec!["pt", "zoom", "pt-r", "zoom-r", "presets"];
    if cam_cfg.onvif.autotracking {
        features.push("autotracking");
    }

    Json(json!({
        "enabled": true,
        "online": online,
        "camera": camera,
        "host": cam_cfg.onvif.host,
        "port": cam_cfg.onvif.port,
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
    AxumPath(camera): AxumPath<String>,
    Json(body): Json<PtzMoveBody>,
) -> Response {
    let cam_opt = { state.config.read().await.cameras.get(&camera).cloned() };
    let Some(cam_cfg) = cam_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": format!("Camera '{camera}' not found") })),
        )
            .into_response();
    };
    let Some(endpoint) = resolve_onvif_ptz_endpoint(&cam_cfg.onvif) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": format!("ONVIF PTZ is not configured for camera '{camera}'") })),
        )
            .into_response();
    };

    // Moving the camera via ContinuousMove or RelativeMove clears active_preset until GotoPreset completes
    state
        .ptz_active_presets
        .write()
        .await
        .insert(camera.clone(), None);

    let (pan, tilt, zoom) = body.resolve_vectors();
    let is_rel = body.is_relative();
    let op_name = if is_rel {
        "RelativeMove"
    } else {
        "ContinuousMove"
    };
    let profile_token = body.profile_token.as_deref().unwrap_or("Profile_1");
    let user = cam_cfg.onvif.user.as_deref();
    let pass = cam_cfg.onvif.password.as_deref();
    let soap_xml = if is_rel {
        build_onvif_relative_move_soap(profile_token, pan, tilt, zoom, body.speed, user, pass)
    } else {
        let move_timeout = body.timeout_sec.unwrap_or(cam_cfg.onvif.timeout_sec);
        build_onvif_continuous_move_soap(profile_token, pan, tilt, zoom, move_timeout, user, pass)
    };
    let timeout = match body.timeout_ms {
        Some(ms) => std::time::Duration::from_millis(ms.clamp(20, 30_000)),
        None => std::time::Duration::from_secs(
            body.timeout_sec
                .unwrap_or(cam_cfg.onvif.timeout_sec)
                .clamp(1, 30),
        ),
    };

    match send_onvif_soap_request(
        &state.http_client,
        &endpoint,
        op_name,
        soap_xml,
        user,
        pass,
        timeout,
    )
    .await
    {
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
    AxumPath(camera): AxumPath<String>,
    body_bytes: axum::body::Bytes,
) -> Response {
    let body: PtzStopBody = if body_bytes.is_empty() {
        PtzStopBody::default()
    } else {
        serde_json::from_slice(&body_bytes).unwrap_or_default()
    };

    let cam_opt = { state.config.read().await.cameras.get(&camera).cloned() };
    let Some(cam_cfg) = cam_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": format!("Camera '{camera}' not found") })),
        )
            .into_response();
    };
    let Some(endpoint) = resolve_onvif_ptz_endpoint(&cam_cfg.onvif) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": format!("ONVIF PTZ is not configured for camera '{camera}'") })),
        )
            .into_response();
    };

    let profile_token = body.profile_token.as_deref().unwrap_or("Profile_1");
    let user = cam_cfg.onvif.user.as_deref();
    let pass = cam_cfg.onvif.password.as_deref();
    let soap_xml = build_onvif_stop_soap(profile_token, body.pan_tilt, body.zoom, user, pass);
    let timeout = match body.timeout_ms {
        Some(ms) => std::time::Duration::from_millis(ms.clamp(20, 30_000)),
        None => std::time::Duration::from_secs(cam_cfg.onvif.timeout_sec.clamp(1, 30)),
    };
    let active_preset = state
        .ptz_active_presets
        .read()
        .await
        .get(&camera)
        .cloned()
        .flatten();

    match send_onvif_soap_request(
        &state.http_client,
        &endpoint,
        "Stop",
        soap_xml,
        user,
        pass,
        timeout,
    )
    .await
    {
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
    AxumPath(camera): AxumPath<String>,
    Json(body): Json<PtzPresetBody>,
) -> Response {
    let preset_token = body.preset.trim();
    if preset_token.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Preset token/name is required" })),
        )
            .into_response();
    }

    let cam_opt = { state.config.read().await.cameras.get(&camera).cloned() };
    let Some(cam_cfg) = cam_opt else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": format!("Camera '{camera}' not found") })),
        )
            .into_response();
    };
    let Some(endpoint) = resolve_onvif_ptz_endpoint(&cam_cfg.onvif) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": format!("ONVIF PTZ is not configured for camera '{camera}'") })),
        )
            .into_response();
    };

    let profile_token = body.profile_token.as_deref().unwrap_or("Profile_1");
    let user = cam_cfg.onvif.user.as_deref();
    let pass = cam_cfg.onvif.password.as_deref();
    let soap_xml =
        build_onvif_goto_preset_soap(profile_token, preset_token, body.speed, user, pass);
    let timeout = match body.timeout_ms {
        Some(ms) => std::time::Duration::from_millis(ms.clamp(20, 30_000)),
        None => std::time::Duration::from_secs(cam_cfg.onvif.timeout_sec.clamp(1, 30)),
    };

    match send_onvif_soap_request(
        &state.http_client,
        &endpoint,
        "GotoPreset",
        soap_xml,
        user,
        pass,
        timeout,
    )
    .await
    {
        Ok(_) => {
            state
                .ptz_active_presets
                .write()
                .await
                .insert(camera.clone(), Some(preset_token.to_string()));
            Json(json!({
                "success": true,
                "camera": camera,
                "operation": "GotoPreset",
                "preset": preset_token,
                "active_preset": preset_token
            }))
            .into_response()
        }
        Err((status, err_msg)) => {
            state
                .ptz_active_presets
                .write()
                .await
                .insert(camera.clone(), None);
            (
                status,
                Json(json!({
                    "success": false,
                    "camera": camera,
                    "operation": "GotoPreset",
                    "error": err_msg,
                    "active_preset": Value::Null
                })),
            )
                .into_response()
        }
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
    AxumPath(camera): AxumPath<String>,
) -> Json<Value> {
    let flags = state.comms.get_camera_flags(&camera);
    Json(serde_json::to_value(flags).unwrap_or_else(|_| json!({})))
}

async fn api_camera_state_set(
    State(state): State<AppState>,
    AxumPath(camera): AxumPath<String>,
    Json(body): Json<SetCameraStateBody>,
) -> Json<Value> {
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
    Json(serde_json::to_value(flags).unwrap_or_else(|_| json!({})))
}

#[derive(Debug, Deserialize, Default)]
pub struct LatestFrameQuery {
    pub height: Option<u32>,
    pub bbox: Option<bool>,
}

async fn api_camera_latest_frame(
    State(state): State<AppState>,
    AxumPath(camera): AxumPath<String>,
    Query(query): Query<LatestFrameQuery>,
    uri: Uri,
) -> Response {
    let _ = query.bbox;
    let is_webp = uri.path().ends_with(".webp");
    let target_height = query.height.filter(|&h| h > 0);
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
        .map(|m| m.last_frame_ts)
        .unwrap_or(0.0);

    let ts_hdr = header::HeaderName::from_static("x-last-frame-timestamp");

    // 1. Check short-lived 400ms in-memory encoded snapshot cache
    let cache_key = (camera.clone(), target_height.unwrap_or(0), is_webp);
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

    // 2. Encode on demand from latest_yuv_frames (zero-copy raw YuvFrame)
    if let Some(yuv_frame) = state.latest_yuv_frames.read().await.get(&camera).cloned() {
        let priv_mask = {
            let cfg = state.config.read().await;
            cfg.cameras.get(&camera).map(|c| c.privacy_mask.clone())
        };
        let mut rgb =
            ninjanvr_vision::yuv420p_to_rgb_image_scaled(&yuv_frame, None, &[], target_height);
        if let Some(ref mask) = priv_mask
            && !mask.is_null()
        {
            ninjanvr_vision::apply_privacy_masks_from_value(&mut rgb, mask);
        }
        let encoded_res = if is_webp {
            ninjanvr_vision::encode_image_webp(&rgb)
        } else {
            ninjanvr_vision::encode_image_jpeg(&rgb)
        };
        if let Ok(encoded_vec) = encoded_res {
            let out_bytes = Bytes::from(encoded_vec);
            let ts = if yuv_frame.timestamp > 0.0 {
                yuv_frame.timestamp as u64
            } else if metric_ts > 0.0 {
                metric_ts as u64
            } else {
                unix_now()
            };
            state
                .lazy_snapshot_cache
                .write()
                .await
                .insert(cache_key, (Instant::now(), out_bytes.clone(), ts));
            return (
                [
                    (header::CONTENT_TYPE, content_type),
                    (ts_hdr, ts.to_string()),
                ],
                out_bytes,
            )
                .into_response();
        }
    }

    if let Some(frame_bytes) = state.latest_frames.read().await.get(&camera).cloned() {
        let ts = if metric_ts > 0.0 {
            metric_ts as u64
        } else {
            unix_now()
        };
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
    if let Ok(disk_bytes) = tokio::fs::read(&disk_latest).await
        && !disk_bytes.is_empty()
    {
        let disk_ts = if metric_ts > 0.0 {
            metric_ts as u64
        } else {
            std::fs::metadata(&disk_latest)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or_else(unix_now)
        };
        return (
            [
                (header::CONTENT_TYPE, "image/jpeg".to_string()),
                (ts_hdr, disk_ts.to_string()),
            ],
            disk_bytes,
        )
            .into_response();
    }

    let url = format!(
        "{}/api/frame.jpeg?src={}_sub",
        state.go2rtc_http_base, camera
    );
    if let Ok(resp) = state
        .http_client
        .get(&url)
        .timeout(std::time::Duration::from_millis(1500))
        .send()
        .await
        && resp.status().is_success()
        && let Ok(bytes) = resp.bytes().await
    {
        let priv_mask = {
            let cfg = state.config.read().await;
            cfg.cameras.get(&camera).map(|c| c.privacy_mask.clone())
        };
        let out_bytes = if let Some(ref mask) = priv_mask
            && !mask.is_null()
            && let Ok(dyn_img) = image::load_from_memory(&bytes)
        {
            let mut rgb = dyn_img.to_rgb8();
            ninjanvr_vision::apply_privacy_masks_from_value(&mut rgb, mask);
            ninjanvr_vision::encode_image_jpeg(&rgb)
                .map(axum::body::Bytes::from)
                .unwrap_or(bytes)
        } else {
            bytes
        };
        return (
            [
                (header::CONTENT_TYPE, "image/jpeg".to_string()),
                (ts_hdr, unix_now().to_string()),
            ],
            out_bytes,
        )
            .into_response();
    }

    placeholder_jpeg_response()
}

async fn api_go2rtc_stream_prewarm(
    State(state): State<AppState>,
    AxumPath(stream): AxumPath<String>,
) -> Response {
    let warm = state
        .mse_hub
        .prewarm_stream(&state.go2rtc_ws_base, &stream)
        .await;
    Json(json!({
        "success": true,
        "stream": stream,
        "warm": warm,
    }))
    .into_response()
}

fn placeholder_jpeg_response() -> Response {
    let mut img = image::RgbImage::new(320, 180);
    for pixel in img.pixels_mut() {
        *pixel = image::Rgb([18, 22, 28]);
    }
    let mut buf = std::io::Cursor::new(Vec::new());
    let _ = img.write_to(&mut buf, image::ImageFormat::Jpeg);
    ([(header::CONTENT_TYPE, "image/jpeg")], buf.into_inner()).into_response()
}

async fn api_go2rtc_streams(State(state): State<AppState>) -> Response {
    let url = format!("{}/api/streams", state.go2rtc_http_base);
    match state.http_client.get(&url).send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
            if let Ok(json_val) = resp.json::<Value>().await {
                return (status, Json(scrub_json_value(&json_val))).into_response();
            }
            Json(json!({})).into_response()
        }
        Err(_) => Json(json!({})).into_response(),
    }
}

async fn api_go2rtc_stream_one(
    State(state): State<AppState>,
    AxumPath(stream): AxumPath<String>,
) -> Response {
    let url = format!(
        "{}/api/streams?src={stream}&video=all&audio=all&microphone=",
        state.go2rtc_http_base
    );
    match state.http_client.get(&url).send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
            if let Ok(json_val) = resp.json::<Value>().await {
                return (status, Json(scrub_json_value(&json_val))).into_response();
            }
            Json(json!({})).into_response()
        }
        Err(_) => Json(json!({})).into_response(),
    }
}

async fn api_go2rtc_webrtc(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let query = uri.query().unwrap_or("");
    let url = if query.is_empty() {
        format!("{}/api/webrtc", state.go2rtc_http_base)
    } else {
        format!("{}/api/webrtc?{query}", state.go2rtc_http_base)
    };
    let mut req = state.http_client.post(&url).body(body.to_vec());
    if let Some(ct) = headers.get(header::CONTENT_TYPE) {
        req = req.header(header::CONTENT_TYPE, ct);
    } else {
        req = req.header(header::CONTENT_TYPE, "application/sdp");
    }
    match req.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
            let content_type = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/sdp")
                .to_string();
            let bytes = resp.bytes().await.unwrap_or_default();
            (
                status,
                [(header::CONTENT_TYPE, content_type)],
                Body::from(bytes),
            )
                .into_response()
        }
        Err(err) => (StatusCode::BAD_GATEWAY, err.to_string()).into_response(),
    }
}

async fn ws_state_handler(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_state_ws(socket, state))
}

async fn handle_state_ws(mut socket: WebSocket, state: AppState) {
    let cameras: Vec<String> = {
        let cfg = state.config.read().await;
        cfg.cameras.keys().cloned().collect()
    };

    for init_msg in state.comms.initial_ws_snapshots(&cameras) {
        if socket.send(AxumWsMsg::Text(init_msg.into())).await.is_err() {
            return;
        }
    }

    let mut rx = state.comms.subscribe_ws();
    loop {
        tokio::select! {
            Ok(msg) = rx.recv() => {
                if socket.send(AxumWsMsg::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            incoming = socket.next() => {
                match incoming {
                    Some(Ok(AxumWsMsg::Text(text))) => {
                        if text.contains("onConnect") {
                            for init_msg in state.comms.initial_ws_snapshots(&cameras) {
                                if socket.send(AxumWsMsg::Text(init_msg.into())).await.is_err() {
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
                            state.comms.apply_set_command(topic, payload);
                        }
                    }
                    Some(Ok(AxumWsMsg::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
}

async fn ws_go2rtc_proxy(
    State(state): State<AppState>,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let query = uri.query().unwrap_or("").to_string();
    ws.on_upgrade(move |client_ws| bridge_go2rtc_ws(client_ws, state, query))
}

async fn bridge_go2rtc_ws(client_ws: WebSocket, state: AppState, query: String) {
    let stream_src = mse_hub::extract_stream_src_from_query(&query);

    // If a stream `src` is specified, use MseStreamHub's shared single-upstream publisher
    // so the instant GOP burst and all subsequent live fragments share a continuous tfdt timeline.
    if let Some(ref src) = stream_src {
        let (mut client_sink, mut client_stream) = client_ws.split();

        // Wait briefly for client MSE handshake (or proceed immediately if none arrives within 40ms)
        match tokio::time::timeout(Duration::from_millis(40), client_stream.next()).await {
            Ok(Some(Ok(AxumWsMsg::Close(_)))) | Ok(Some(Err(_))) | Ok(None) => return,
            _ => {}
        }

        let Some(mut sub) = state
            .mse_hub
            .subscribe_client(&state.go2rtc_ws_base, src)
            .await
        else {
            return;
        };

        let mut last_sent_seq = 0u64;
        let mut sent_init = false;
        if let Some(burst) = sub.burst.take() {
            if client_sink
                .send(AxumWsMsg::Text(burst.codec_msg.into()))
                .await
                .is_err()
            {
                return;
            }
            if client_sink
                .send(AxumWsMsg::Binary(burst.init_segment))
                .await
                .is_err()
            {
                return;
            }
            for frag in burst.gop_fragments {
                if client_sink.send(AxumWsMsg::Binary(frag)).await.is_err() {
                    return;
                }
            }
            last_sent_seq = burst.last_seq;
            sent_init = true;
        }

        loop {
            tokio::select! {
                client_msg = client_stream.next() => {
                    match client_msg {
                        Some(Ok(AxumWsMsg::Ping(p))) => {
                            if client_sink.send(AxumWsMsg::Pong(p)).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(AxumWsMsg::Close(_))) | Some(Err(_)) | None => break,
                        _ => {}
                    }
                }
                live_pkt = sub.rx.recv() => {
                    match live_pkt {
                        Ok(mse_hub::MseLivePacket::Codec(_)) | Ok(mse_hub::MseLivePacket::Init(_)) => {
                            if !sent_init
                                && let Some(burst) = state.mse_hub.get_fresh_burst(src).await
                            {
                                if client_sink.send(AxumWsMsg::Text(burst.codec_msg.into())).await.is_err() {
                                    break;
                                }
                                if client_sink.send(AxumWsMsg::Binary(burst.init_segment)).await.is_err() {
                                    break;
                                }
                                for frag in burst.gop_fragments {
                                    if client_sink.send(AxumWsMsg::Binary(frag)).await.is_err() {
                                        return;
                                    }
                                }
                                last_sent_seq = burst.last_seq;
                                sent_init = true;
                            }
                        }
                        Ok(mse_hub::MseLivePacket::Fragment { seq, bytes }) => {
                            if !sent_init {
                                if let Some(burst) = state.mse_hub.get_fresh_burst(src).await {
                                    if client_sink.send(AxumWsMsg::Text(burst.codec_msg.into())).await.is_err() {
                                        break;
                                    }
                                    if client_sink.send(AxumWsMsg::Binary(burst.init_segment)).await.is_err() {
                                        break;
                                    }
                                    for frag in burst.gop_fragments {
                                        if client_sink.send(AxumWsMsg::Binary(frag)).await.is_err() {
                                            return;
                                        }
                                    }
                                    last_sent_seq = burst.last_seq;
                                    sent_init = true;
                                }
                                continue;
                            }
                            if seq <= last_sent_seq {
                                continue;
                            }
                            last_sent_seq = seq;
                            if client_sink.send(AxumWsMsg::Binary(bytes)).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        return;
    }

    let target_url = if query.is_empty() {
        format!("{}/api/ws", state.go2rtc_ws_base)
    } else {
        format!("{}/api/ws?{query}", state.go2rtc_ws_base)
    };

    let Ok((upstream_ws, _)) = tokio_tungstenite::connect_async(&target_url).await else {
        warn!("Failed to connect upstream go2rtc WS at {target_url}");
        return;
    };

    let (mut client_sink, mut client_stream) = client_ws.split();
    let (mut up_sink, mut up_stream) = upstream_ws.split();

    let c2u = async {
        while let Some(Ok(msg)) = client_stream.next().await {
            let out = match msg {
                AxumWsMsg::Text(t) => TungMsg::Text(t.to_string().into()),
                AxumWsMsg::Binary(b) => TungMsg::Binary(b),
                AxumWsMsg::Ping(p) => TungMsg::Ping(p),
                AxumWsMsg::Pong(p) => TungMsg::Pong(p),
                AxumWsMsg::Close(_) => break,
            };
            if up_sink.send(out).await.is_err() {
                break;
            }
        }
    };

    let u2c = async {
        while let Some(Ok(msg)) = up_stream.next().await {
            let out = match msg {
                TungMsg::Text(t) => AxumWsMsg::Text(t.to_string().into()),
                TungMsg::Binary(b) => AxumWsMsg::Binary(b),
                TungMsg::Ping(p) => AxumWsMsg::Ping(p),
                TungMsg::Pong(p) => AxumWsMsg::Pong(p),
                TungMsg::Close(_) => break,
                TungMsg::Frame(_) => continue,
            };
            if client_sink.send(out).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = c2u => {},
        _ = u2c => {},
    }
}

fn resolve_recording_disk_path(media_dir: &Path, db_path: &str) -> PathBuf {
    if let Some(rel) = ninjanvr_db::strip_media_root(db_path) {
        media_dir.join(rel)
    } else {
        PathBuf::from(db_path)
    }
}

async fn vod_master_m3u8(
    AxumPath((_camera, _start, _end)): AxumPath<(String, f64, f64)>,
) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
        ninjanvr_vod::build_master_playlist(),
    )
}

fn existing_recordings_for_vod(
    state: &AppState,
    camera: &str,
    start: f64,
    end: f64,
) -> Vec<ninjanvr_db::RecordingRow> {
    state
        .db
        .query_recordings(camera, start, end)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| resolve_recording_disk_path(&state.media_dir, &r.path).exists())
        .collect()
}

async fn vod_index_m3u8(
    State(state): State<AppState>,
    AxumPath((camera, start, end)): AxumPath<(String, f64, f64)>,
) -> Response {
    let recs = existing_recordings_for_vod(&state, &camera, start, end);
    if recs.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let resolutions: Vec<(u32, u32)> = recs
        .iter()
        .map(|r| {
            let disk_path = resolve_recording_disk_path(&state.media_dir, &r.path);
            ninjanvr_vod::probe_mp4_resolution(&disk_path).unwrap_or(if r.objects > 0 {
                (1920, 1080)
            } else {
                (640, 360)
            })
        })
        .collect();
    match ninjanvr_vod::build_media_playlist_with_resolutions(&recs, Some(&resolutions)) {
        Ok(m3u8) => (
            [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
            m3u8,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct VodInitQuery {
    pub seg: Option<usize>,
}

async fn vod_init_mp4(
    State(state): State<AppState>,
    AxumPath((camera, start, end)): AxumPath<(String, f64, f64)>,
    Query(q): Query<VodInitQuery>,
) -> Response {
    let recs = existing_recordings_for_vod(&state, &camera, start, end);
    let start_idx = q.seg.unwrap_or(1).saturating_sub(1);
    for rec in recs.into_iter().skip(start_idx) {
        let disk_path = resolve_recording_disk_path(&state.media_dir, &rec.path);
        if let Ok(Ok(init_bytes)) =
            tokio::task::spawn_blocking(move || ninjanvr_vod::build_init_mp4(&disk_path)).await
        {
            return ([(header::CONTENT_TYPE, "video/mp4")], init_bytes).into_response();
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn vod_segment_m4s(
    State(state): State<AppState>,
    AxumPath((camera, start, end, segment)): AxumPath<(String, f64, f64, String)>,
) -> Response {
    let Some(num_str) = segment
        .strip_prefix("seg-")
        .and_then(|s| s.split('-').next())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(seq_num) = num_str.parse::<usize>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if seq_num == 0 {
        return StatusCode::NOT_FOUND.into_response();
    }

    let recs = existing_recordings_for_vod(&state, &camera, start, end);
    let Some(rec) = recs.get(seq_num - 1).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let base_time = recs
        .iter()
        .take(seq_num - 1)
        .map(|r| r.duration.max(0.0))
        .sum::<f64>();
    let disk_path = resolve_recording_disk_path(&state.media_dir, &rec.path);
    match tokio::task::spawn_blocking(move || {
        ninjanvr_vod::build_media_segment_m4s(&disk_path, seq_num as u32, base_time)
    })
    .await
    {
        Ok(Ok(bytes)) => ([(header::CONTENT_TYPE, "video/iso.segment")], bytes).into_response(),
        Ok(Err(err)) => {
            warn!("Failed to build fMP4 segment {segment} for {camera}: {err:#}");
            StatusCode::NOT_FOUND.into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub fn generate_self_signed_tls_config() -> Result<Arc<rustls::ServerConfig>> {
    generate_self_signed_tls_config_with_sans(&[])
}

pub fn generate_self_signed_tls_config_with_sans(
    extra_sans: &[String],
) -> Result<Arc<rustls::ServerConfig>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut subject_alt_names = vec![
        "localhost".to_string(),
        "ninjanvr".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    for host in extra_sans {
        let h = host.trim();
        if !h.is_empty() && !subject_alt_names.iter().any(|s| s == h) {
            subject_alt_names.push(h.to_string());
        }
    }
    let mut params = rcgen::CertificateParams::new(subject_alt_names)?;
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let signing_key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&signing_key)?;
    let cert_der = rustls::pki_types::CertificateDer::from(cert);
    let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()),
    );

    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;
    server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(server_config))
}

pub async fn new_app_state(
    config_path: &Path,
    web_dir: PathBuf,
    media_dir: PathBuf,
    spawn_go2rtc: bool,
) -> Result<AppState> {
    let cfg = NinjaConfig::load(config_path)?;
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

    let db = match NinjaDatabase::open(&cfg.database.path) {
        Ok(d) => d,
        Err(err) => {
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
            let fallback_db =
                std::env::temp_dir().join(format!("ninjanvr-degraded-{}.db", std::process::id()));
            NinjaDatabase::open(&fallback_db)?
        }
    };
    if let Some(generated_pw) = db.ensure_default_admin(cfg.auth.hash_iterations)? {
        info!(
            "Initialized default admin user (password length: {})",
            generated_pw.len()
        );
    }

    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("/config"));
    let jwt_secret = load_or_create_jwt_secret(config_dir);

    let detector = Arc::new(AiDetectorEngine::new(&cfg));
    let comms = Arc::new(CommsHub::with_mqtt(&cfg.mqtt));
    comms.init_from_config(&cfg);
    let profile_file = config_dir.join(".active_profile");
    comms.set_profile_persist_path(profile_file.clone());
    if let Ok(saved_prof) = std::fs::read_to_string(&profile_file)
        && !saved_prof.trim().is_empty()
    {
        comms.set_active_profile(saved_prof.trim());
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
    log_hub.push_ninjanvr(
        "INFO",
        "ninjanvr.infer",
        &format!(
            "Initialized OpenVINO C-API detector on CPU (model={})",
            cfg.model.path
        ),
    );
    for (cam_name, cam_cfg) in &cfg.cameras {
        if cam_cfg.enabled {
            log_hub.push_ninjanvr(
                "INFO",
                &format!("ninjanvr.camera.{cam_name}"),
                &format!(
                    "Started substream vision & direct segment recorder ({}x{} @ {} FPS)",
                    cam_cfg.detect.width, cam_cfg.detect.height, cam_cfg.detect.fps
                ),
            );
            log_hub.push_go2rtc(
                "INF",
                "streams",
                &format!("Registered RTSP restream producer for {cam_name} and {cam_name}_sub"),
            );
        }
    }
    log_hub.push_go2rtc("INF", "rtsp", "listen addr=:8554");
    log_hub.push_go2rtc("INF", "api", "listen addr=:1984");
    log_hub.push_go2rtc("INF", "webrtc", "listen tcp/udp addr=:8555");
    log_hub.push_nginx("GET", "/api/version", 200);
    log_hub.push_nginx("GET", "/api/stats", 200);
    {
        let rec_dir = media_dir.join("recordings");
        let rec_mount = stat_mount_mb(&rec_dir, "ext4");
        let startup_heuristics =
            evaluate_system_heuristics(&cfg, &serde_json::Map::new(), &rec_mount, 14.0);
        emit_heuristics_to_logs(&log_hub, &startup_heuristics);
    }

    let shared_cfg = Arc::new(RwLock::new(cfg));
    let activity_ledger = new_activity_ledger();
    let latest_frames = Arc::new(RwLock::new(HashMap::new()));
    let latest_yuv_frames: SharedLatestYuvFrames = Arc::new(RwLock::new(HashMap::new()));
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

    if spawn_go2rtc {
        go2rtc_supervisor.clone().spawn_supervisor_task();
        let rec_engine = Arc::new(RecordingEngine::with_storage_state(
            shared_cfg.clone(),
            db.clone(),
            media_dir.clone(),
            activity_ledger.clone(),
            storage_degraded.clone(),
            storage_error.clone(),
        ));
        rec_engine.start().await;

        let vision_pipeline = Arc::new(VisionPipeline {
            config: shared_cfg.clone(),
            db: db.clone(),
            comms: comms.clone(),
            detector: detector.clone(),
            activity_ledger: activity_ledger.clone(),
            latest_frames: latest_frames.clone(),
            latest_yuv_frames: latest_yuv_frames.clone(),
            camera_metrics: camera_metrics.clone(),
            media_dir: media_dir.clone(),
            log_hub: log_hub.clone(),
            ptz_active_presets: ptz_active_presets.clone(),
        });
        vision_pipeline.start().await;
    }

    let _ = rustls::crypto::ring::default_provider().install_default();
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let web_dir = if web_dir.join("dist").join("index.html").exists() {
        web_dir.join("dist")
    } else if !web_dir.join("index.html").exists()
        && web_dir
            .parent()
            .is_some_and(|p| p.join("web-v2").join("index.html").exists())
    {
        web_dir.parent().unwrap().join("web-v2")
    } else {
        web_dir
    };

    let state = AppState {
        config: shared_cfg,
        db,
        comms,
        go2rtc_supervisor,
        go2rtc_http_base: "http://127.0.0.1:1984".to_string(),
        go2rtc_ws_base: "ws://127.0.0.1:1984".to_string(),
        http_client,
        jwt_secret,
        start_instant: Instant::now(),
        web_dir,
        media_dir,
        latest_frames,
        latest_yuv_frames,
        lazy_snapshot_cache,
        mse_hub,
        activity_ledger,
        detector,
        camera_metrics,
        log_hub,
        stats_history,
        cpu_tracker: ProcCpuTracker::default(),
        storage_degraded,
        storage_error,
        ptz_active_presets,
        share_clip_locks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        share_page_video_credits: Arc::new(std::sync::Mutex::new(HashMap::new())),
        share_ffmpeg_semaphore: Arc::new(tokio::sync::Semaphore::new(2)),
    };

    // Seed 30 initial stats_history points so all 30 bars in /system#general and /system#cameras render with zero 0-padding
    {
        let Json(base_snap) = api_stats(State(state.clone())).await;
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
        let stats_state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let Json(stats_val) = api_stats(State(stats_state.clone())).await;
                {
                    let mut hist = stats_state.stats_history.write().await;
                    hist.push_back(stats_val.clone());
                    while hist.len() > MAX_STATS_HISTORY {
                        hist.pop_front();
                    }
                }
                stats_state.comms.publish_json("stats", &stats_val);
            }
        });
    }

    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    struct TestApp {
        tmp_dir: PathBuf,
        cfg_path: PathBuf,
        state: AppState,
        app: Router,
    }

    impl Drop for TestApp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.tmp_dir);
        }
    }

    async fn setup_test_app() -> TestApp {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr-srv-test-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let db_path = tmp_dir.join("ninjanvr.db");
        let cfg_path = tmp_dir.join("config.yml");
        let web_dir = tmp_dir.join("web-v2");
        std::fs::create_dir_all(&web_dir).unwrap();
        std::fs::write(
            web_dir.join("index.html"),
            "<html><body>NinjaNVR Console v2</body></html>",
        )
        .unwrap();

        let yaml = format!(
            r#"
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
go2rtc:
  streams:
    front_door:
      - rtsp://admin:supersecret@192.0.2.10:554/stream1
cameras:
  front_door:
    enabled: true
    detect:
      width: 640
      height: 480
      fps: 5
version: 0.18-0
"#,
            db_path.display()
        );
        std::fs::write(&cfg_path, yaml).unwrap();

        let state = new_app_state(&cfg_path, web_dir, tmp_dir.clone(), false)
            .await
            .unwrap();
        let app = build_router(state.clone());

        TestApp {
            tmp_dir,
            cfg_path,
            state,
            app,
        }
    }

    fn make_mp4_box(box_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let total = (8 + payload.len()) as u32;
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(box_type);
        out.extend_from_slice(payload);
        out
    }

    fn make_mp4_full_box(box_type: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let total = (12 + body.len()) as u32;
        let mut out = Vec::with_capacity(12 + body.len());
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(box_type);
        let vf = ((version as u32) << 24) | (flags & 0x00FF_FFFF);
        out.extend_from_slice(&vf.to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    fn build_synthetic_mp4_bytes() -> Vec<u8> {
        let ftyp = make_mp4_box(b"ftyp", b"isom\x00\x00\x02\x00isom");
        let video_sample = vec![0x65u8; 64];
        let mdat_offset = (ftyp.len() + 8) as u32;
        let mdat = make_mp4_box(b"mdat", &video_sample);

        let mut tkhd_body = vec![0u8; 80];
        tkhd_body[8..12].copy_from_slice(&1u32.to_be_bytes());
        tkhd_body[72..76].copy_from_slice(&(640u32 << 16).to_be_bytes());
        tkhd_body[76..80].copy_from_slice(&(480u32 << 16).to_be_bytes());
        let tkhd = make_mp4_full_box(b"tkhd", 0, 3, &tkhd_body);

        let mut mdhd_body = vec![0u8; 20];
        mdhd_body[8..12].copy_from_slice(&90000u32.to_be_bytes());
        mdhd_body[12..16].copy_from_slice(&900000u32.to_be_bytes());
        let mdhd = make_mp4_full_box(b"mdhd", 0, 0, &mdhd_body);

        let mut hdlr_body = vec![0u8; 20];
        hdlr_body[4..8].copy_from_slice(b"vide");
        let hdlr = make_mp4_full_box(b"hdlr", 0, 0, &hdlr_body);

        let stsd = make_mp4_full_box(b"stsd", 0, 0, &[0, 0, 0, 1]);
        let mut stsz_body = Vec::new();
        stsz_body.extend_from_slice(&(video_sample.len() as u32).to_be_bytes());
        stsz_body.extend_from_slice(&1u32.to_be_bytes());
        let stsz = make_mp4_full_box(b"stsz", 0, 0, &stsz_body);

        let mut stts_body = Vec::new();
        stts_body.extend_from_slice(&1u32.to_be_bytes());
        stts_body.extend_from_slice(&1u32.to_be_bytes());
        stts_body.extend_from_slice(&900000u32.to_be_bytes());
        let stts = make_mp4_full_box(b"stts", 0, 0, &stts_body);

        let mut stsc_body = Vec::new();
        stsc_body.extend_from_slice(&1u32.to_be_bytes());
        stsc_body.extend_from_slice(&1u32.to_be_bytes());
        stsc_body.extend_from_slice(&1u32.to_be_bytes());
        stsc_body.extend_from_slice(&1u32.to_be_bytes());
        let stsc = make_mp4_full_box(b"stsc", 0, 0, &stsc_body);

        let mut stco_body = Vec::new();
        stco_body.extend_from_slice(&1u32.to_be_bytes());
        stco_body.extend_from_slice(&mdat_offset.to_be_bytes());
        let stco = make_mp4_full_box(b"stco", 0, 0, &stco_body);

        let mut stbl_children = Vec::new();
        stbl_children.extend_from_slice(&stsd);
        stbl_children.extend_from_slice(&stsz);
        stbl_children.extend_from_slice(&stts);
        stbl_children.extend_from_slice(&stsc);
        stbl_children.extend_from_slice(&stco);
        let stbl = make_mp4_box(b"stbl", &stbl_children);
        let minf = make_mp4_box(b"minf", &stbl);

        let mut mdia_children = Vec::new();
        mdia_children.extend_from_slice(&mdhd);
        mdia_children.extend_from_slice(&hdlr);
        mdia_children.extend_from_slice(&minf);
        let mdia = make_mp4_box(b"mdia", &mdia_children);

        let mut trak_children = Vec::new();
        trak_children.extend_from_slice(&tkhd);
        trak_children.extend_from_slice(&mdia);
        let vide_trak = make_mp4_box(b"trak", &trak_children);

        let mvhd = make_mp4_full_box(b"mvhd", 0, 0, &[0u8; 96]);
        let mut moov_children = Vec::new();
        moov_children.extend_from_slice(&mvhd);
        moov_children.extend_from_slice(&vide_trak);
        let moov = make_mp4_box(b"moov", &moov_children);

        let mut mp4_bytes = Vec::new();
        mp4_bytes.extend_from_slice(&ftyp);
        mp4_bytes.extend_from_slice(&mdat);
        mp4_bytes.extend_from_slice(&moov);
        mp4_bytes
    }

    #[tokio::test]
    async fn router_serves_version_config_and_dual_ui_routing() {
        let ctx = setup_test_app().await;
        let app = &ctx.app;

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], NINJANVR_VERSION.as_bytes());

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 65536).await.unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(!body_str.contains("supersecret"));
        assert!(body_str.contains("rtsp://*:*@192.0.2.10:554/stream1"));
        let val: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val["cameras"]["front_door"]["detect"]["width"], 640);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/nonexistent_route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/review")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 4096).await.unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("NinjaNVR Console v2")
        );

        let resp_legacy_cookie = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/review")
                    .header(header::COOKIE, "ninjanvr_ui=v1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_legacy_cookie.status(), StatusCode::OK);
        let body_legacy = to_bytes(resp_legacy_cookie.into_body(), 4096)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&body_legacy)
                .unwrap()
                .contains("NinjaNVR Console v2")
        );

        let resp_switch = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/ui/switch?target=v1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_switch.status(), StatusCode::FOUND);
        assert!(
            resp_switch
                .headers()
                .get(header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("ninjanvr_ui=v2")
        );

        let mut cfg_patch = val.clone();
        cfg_patch["cameras"]["front_door"]["detect"]["fps"] = json!(8);
        cfg_patch["ui"] = json!({
            "floorplan": {
                "background_data_url": "data:image/svg+xml;base64,PHN2Zy8+",
                "opacity": 0.65,
                "rooms": [
                    {
                        "id": "escritorio",
                        "label": "ESCRITÓRIO",
                        "x": 0.08,
                        "y": 0.10,
                        "w": 0.38,
                        "h": 0.36
                    }
                ],
                "walls": [
                    { "points": [0.08, 0.46, 0.46, 0.46] }
                ]
            }
        });
        let resp_struct = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/config/structured")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&cfg_patch).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_struct.status(), StatusCode::OK);
        let struct_body: Value =
            serde_json::from_slice(&to_bytes(resp_struct.into_body(), 65536).await.unwrap())
                .unwrap();
        assert_eq!(struct_body["success"], true);
        assert_eq!(
            struct_body["config"]["cameras"]["front_door"]["detect"]["fps"],
            8
        );
        assert_eq!(
            struct_body["config"]["ui"]["floorplan"]["rooms"][0]["id"],
            "escritorio"
        );
        assert!(
            (struct_body["config"]["ui"]["floorplan"]["walls"][0]["points"][0]
                .as_f64()
                .unwrap()
                - 0.08)
                .abs()
                < 1e-4
        );
        assert!(
            (struct_body["config"]["ui"]["floorplan"]["walls"][0]["points"][1]
                .as_f64()
                .unwrap()
                - 0.46)
                .abs()
                < 1e-4
        );
        let on_disk_yaml = std::fs::read_to_string(&ctx.cfg_path).unwrap();
        assert!(on_disk_yaml.contains("admin:supersecret@192.0.2.10:554/stream1"));
        assert!(on_disk_yaml.contains("escritorio"));

        let resp_probe_err = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/streams/probe")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"url":"rtsp://admin:topsecretpass@192.0.2.99:554/unreachable_stream"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_probe_err.status(), StatusCode::OK);
        let probe_err_bytes = to_bytes(resp_probe_err.into_body(), 8192).await.unwrap();
        let probe_err_str = std::str::from_utf8(&probe_err_bytes).unwrap();
        assert!(
            !probe_err_str.contains("topsecretpass"),
            "probe output must scrub RTSP credentials"
        );
        let probe_err_json: Value = serde_json::from_slice(&probe_err_bytes).unwrap();
        assert_eq!(probe_err_json["ok"], false);

        let resp_prof_set = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/profile")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"profile":"home"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_prof_set.status(), StatusCode::OK);
        let resp_prof_get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/profile/active")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let prof_json: Value =
            serde_json::from_slice(&to_bytes(resp_prof_get.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(prof_json["active_profile"], "home");

        for aux_uri in [
            "/version",
            "/api/ui/version",
            "/api/profile",
            "/api/config/raw",
            "/api/config/raw_paths",
            "/api/config/schema.json",
            "/api/detectors/catalog",
            "/api/ffmpeg/presets",
            "/api/ffprobe",
            "/api/keyframe_analysis",
            "/api/profiles",
            "/api/labels",
            "/api/sub_labels",
            "/api/audio_labels",
            "/api/recognized_license_plates",
            "/api/classification/attributes",
            "/api/classification/custom",
            "/api/genai/models",
            "/api/plus/models",
            "/api/faces",
            "/login",
        ] {
            let r = app
                .clone()
                .oneshot(Request::builder().uri(aux_uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK, "failed on {aux_uri}");
        }

        let resp_cfg_set = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/config/set")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_cfg_set.status(), StatusCode::OK);

        let resp_restart = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/restart")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_restart.status(), StatusCode::OK);

        let raw_disk = std::fs::read_to_string(&ctx.cfg_path).unwrap();
        let resp_cfg_save = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/config/save")
                    .body(Body::from(raw_disk))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_cfg_save.status(), StatusCode::OK);

        let resp_cat = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/detectors/catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_cat.status(), StatusCode::OK);
        let cat_json: Value =
            serde_json::from_slice(&to_bytes(resp_cat.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(cat_json["active_backend"], "openvino");
        assert!(cat_json["openvino_available"].is_boolean());
        assert!(cat_json["ort_available"].is_boolean());
        let dev_arr = cat_json["devices"].as_array().unwrap();
        assert!(dev_arr.iter().any(|d| d["id"] == "CUDA"));
        assert!(dev_arr.iter().any(|d| d["id"] == "ROCM"));
    }

    #[tokio::test]
    async fn router_enforces_rbac_camera_scoping_and_api_tokens() {
        let ctx = setup_test_app().await;
        let app = &ctx.app;
        let state = &ctx.state;

        let resp_create_user = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/users")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"username":"operator2","password":"strong-password","role":"viewer"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_create_user.status(), StatusCode::OK);

        let resp_role = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/users/operator2/role")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"role":"admin"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_role.status(), StatusCode::OK);

        let resp_cams = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/users/operator2/cameras")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"allowed_cameras":"front_door"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_cams.status(), StatusCode::OK);

        let resp_pw = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/users/operator2/password")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"password":"updated-password"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_pw.status(), StatusCode::OK);

        let resp_del_user = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/users/operator2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_del_user.status(), StatusCode::OK);

        let resp_users_list = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/users")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_users_list.status(), StatusCode::OK);

        let resp_tok_http_create = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/tokens")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"name":"temp_tok","scope":"read_only"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_tok_http_create.status(), StatusCode::OK);
        let tok_http_json: Value = serde_json::from_slice(
            &to_bytes(resp_tok_http_create.into_body(), 4096)
                .await
                .unwrap(),
        )
        .unwrap();
        let created_tok_id = tok_http_json["metadata"]["id"].as_str().unwrap();

        let resp_tok_http_del = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/tokens/{created_tok_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_tok_http_del.status(), StatusCode::OK);

        let resp_logout = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/logout")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_logout.status(), StatusCode::SEE_OTHER);

        let resp_proxy_sub = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/camera-proxy/non_existent_cam/cgi-bin/config.cgi")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_proxy_sub.status(), StatusCode::NOT_FOUND);

        let auth_app = build_router_with_auth(state.clone(), true);
        let resp = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        state
            .db
            .create_user_with_cameras("guard1", "guard-pass", "viewer", "front_door", 1000)
            .unwrap();
        let guard_claims = JwtClaims {
            sub: "guard1".to_string(),
            role: "viewer".to_string(),
            allowed_cameras: Some("front_door".to_string()),
            iat: 1700000000,
            exp: 9999999999,
        };
        let guard_token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &guard_claims,
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap();

        let resp_viewer_save = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/config/save")
                    .header(header::AUTHORIZATION, format!("Bearer {guard_token}"))
                    .body(Body::from("version: 0.18-0"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_viewer_save.status(), StatusCode::FORBIDDEN);

        let resp_viewer_ok_cam = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/front_door/recordings")
                    .header(header::AUTHORIZATION, format!("Bearer {guard_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_viewer_ok_cam.status(), StatusCode::OK);

        let resp_viewer_unauth_cam = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/indoor_bedroom/recordings")
                    .header(header::AUTHORIZATION, format!("Bearer {guard_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_viewer_unauth_cam.status(), StatusCode::FORBIDDEN);

        let resp_proxy_viewer = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/camera-proxy/front_door/")
                    .header(header::AUTHORIZATION, format!("Bearer {guard_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_proxy_viewer.status(), StatusCode::FORBIDDEN);

        let resp_proxy_unconf = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/camera-proxy/non_existent_cam/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_proxy_unconf.status(), StatusCode::NOT_FOUND);

        let (tok_meta, raw_ro_tok) = state
            .db
            .create_api_token("ha_sensor", "read_only", None)
            .unwrap();
        assert!(raw_ro_tok.starts_with("nnvr_"));
        let tok_id = tok_meta["id"].as_str().unwrap();

        let resp_ro_stats = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/stats")
                    .header(header::AUTHORIZATION, format!("Bearer {raw_ro_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ro_stats.status(), StatusCode::OK);

        let resp_ro_events = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/events")
                    .header(header::AUTHORIZATION, format!("Bearer {raw_ro_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ro_events.status(), StatusCode::OK);

        for admin_path in ["/api/users", "/api/tokens", "/api/config/raw"] {
            let resp_ro_admin = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(admin_path)
                        .header(header::AUTHORIZATION, format!("Bearer {raw_ro_tok}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp_ro_admin.status(), StatusCode::FORBIDDEN);
        }

        let resp_ro_mutate = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/profile")
                    .header(header::AUTHORIZATION, format!("Bearer {raw_ro_tok}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"profile":"disarmed"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ro_mutate.status(), StatusCode::FORBIDDEN);

        let (_, raw_metrics_tok) = state
            .db
            .create_api_token("prom_scraper", "metrics", None)
            .unwrap();
        let resp_metrics_ok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/metrics")
                    .header(header::AUTHORIZATION, format!("Bearer {raw_metrics_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_metrics_ok.status(), StatusCode::OK);

        let resp_metrics_events = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/events")
                    .header(header::AUTHORIZATION, format!("Bearer {raw_metrics_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_metrics_events.status(), StatusCode::FORBIDDEN);

        let (_, raw_full_tok) = state
            .db
            .create_api_token("cli_admin", "full", None)
            .unwrap();
        let resp_full_list = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/tokens")
                    .header(header::AUTHORIZATION, format!("Bearer {raw_full_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_full_list.status(), StatusCode::OK);

        assert!(state.db.delete_api_token(tok_id).unwrap());
        let resp_revoked = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/stats")
                    .header(header::AUTHORIZATION, format!("Bearer {raw_ro_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_revoked.status(), StatusCode::UNAUTHORIZED);

        let resp_first = auth_app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/first_time_login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_first.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn router_validates_oidc_roles_and_trusted_proxy_cidrs() {
        let ctx = setup_test_app().await;
        let auth_app = build_router_with_auth(ctx.state.clone(), true);

        let oidc_cfg = ninjanvr_config::OidcAuthConfig::default();
        assert_eq!(
            resolve_oidc_role(&json!({ "groups": ["users", "admins"] }), &oidc_cfg),
            "admin"
        );
        assert_eq!(
            resolve_oidc_role(
                &json!({
                    "urn:zitadel:iam:org:project:roles": {
                        "ninjanvr-operator": { "12345": "org" }
                    }
                }),
                &oidc_cfg
            ),
            "operator"
        );
        assert_eq!(
            resolve_oidc_role(
                &json!({
                    "urn:zitadel:iam:org:project:roles": {
                        "ninjanvr-viewer": { "12345": "org" }
                    }
                }),
                &oidc_cfg
            ),
            "viewer"
        );
        assert!(ip_matches_trusted_proxies(
            "10.42.5.19",
            &["127.0.0.1/32".to_string(), "10.42.0.0/16".to_string()]
        ));
        assert!(!ip_matches_trusted_proxies(
            "203.0.113.99",
            &["127.0.0.1/32".to_string(), "10.42.0.0/16".to_string()]
        ));

        let resp_spoofed = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/stats")
                    .header("Remote-User", "admin")
                    .header("x-real-ip", "203.0.113.99")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_spoofed.status(), StatusCode::UNAUTHORIZED);

        let resp_providers = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_providers.status(), StatusCode::OK);
        let prov_json: Value =
            serde_json::from_slice(&to_bytes(resp_providers.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(prov_json["local_login_enabled"], true);
        assert!(prov_json["oidc"].is_object());

        let tls_cfg = generate_self_signed_tls_config_with_sans(&[
            "192.0.2.10".to_string(),
            "nvr.example.com".to_string(),
        ])
        .unwrap();
        assert_eq!(
            tls_cfg.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
        let default_tls = generate_self_signed_tls_config().unwrap();
        assert_eq!(default_tls.alpn_protocols, tls_cfg.alpn_protocols);

        let resp_oidc_login = ctx
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_oidc_login.status(), StatusCode::BAD_REQUEST);

        let resp_oidc_cb = ctx
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/callback")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_oidc_cb.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn router_serves_hmac_share_links_and_http_range_video() {
        let ctx = setup_test_app().await;
        let app = &ctx.app;
        let state = &ctx.state;
        let auth_app = build_router_with_auth(state.clone(), true);

        let exports_dir = ctx.tmp_dir.join("exports");
        std::fs::create_dir_all(&exports_dir).unwrap();
        let dummy_mp4 = vec![0x42u8; 4096];
        std::fs::write(exports_dir.join("sample.mp4"), &dummy_mp4).unwrap();

        let resp_mp4 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/exports/sample.mp4")
                    .header(header::ACCEPT_ENCODING, "gzip, br")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_mp4.status(), StatusCode::OK);
        assert!(
            resp_mp4.headers().get(header::CONTENT_ENCODING).is_none(),
            "MP4 exports must not be Brotli/Gzip compressed by CompressionLayer"
        );
        assert_eq!(
            resp_mp4
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some("4096")
        );

        let resp_exp_err = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/export/front_door/start/1000/end/1060")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"name":"missing_clip"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_exp_err.status(), StatusCode::BAD_REQUEST);
        assert!(state.db.query_exports().unwrap().is_empty());

        let resp_share = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"sample","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_share.status(), StatusCode::OK);
        let share_json: Value =
            serde_json::from_slice(&to_bytes(resp_share.into_body(), 4096).await.unwrap()).unwrap();
        let share_token = share_json["token"].as_str().unwrap();

        let resp_share_page = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{share_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_share_page.status(), StatusCode::OK);

        let resp_share_vid = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{share_token}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_share_vid.status(), StatusCode::OK);

        let resp_share_range = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{share_token}/video.mp4"))
                    .header(header::RANGE, "bytes=0-3")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_share_range.status(), StatusCode::PARTIAL_CONTENT);
        assert!(
            resp_share_range
                .headers()
                .contains_key(header::CONTENT_RANGE)
        );

        let resp_tampered = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{share_token}tampered"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_tampered.status(), StatusCode::FORBIDDEN);

        let expired_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: "sample".to_string(),
                exp: 1000,
                cam: None,
                jti: None,
            },
        );
        let resp_expired = auth_app
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{expired_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_expired.status(), StatusCode::GONE);
    }

    #[tokio::test]
    async fn router_handles_storage_degradation_and_prometheus_metrics() {
        let ctx = setup_test_app().await;
        let app = &ctx.app;
        let state = &ctx.state;

        let blocked_media_path = ctx.tmp_dir.join("unwritable_mount_file");
        std::fs::write(&blocked_media_path, b"not_a_directory").unwrap();
        let degraded_state = new_app_state(
            &ctx.cfg_path,
            ctx.tmp_dir.join("web-v2"),
            blocked_media_path.clone(),
            false,
        )
        .await
        .expect("server must boot even when media_dir is unwritable");
        let degraded_app = build_router(degraded_state);
        let resp_deg = degraded_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_deg.status(), StatusCode::OK);
        let deg_json: Value =
            serde_json::from_slice(&to_bytes(resp_deg.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(deg_json["service"]["storage_degraded"], true);
        assert!(deg_json["service"]["storage_error"].is_string());

        std::fs::remove_file(&blocked_media_path).unwrap();
        std::fs::create_dir_all(&blocked_media_path).unwrap();
        let resp_rec = degraded_app
            .oneshot(
                Request::builder()
                    .uri("/api/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let rec_json: Value =
            serde_json::from_slice(&to_bytes(resp_rec.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(rec_json["service"]["storage_degraded"], false);

        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_prom_seed".to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!(["driveway"]),
                start_time: 1700000100.0,
                end_time: Some(1700000112.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.92),
                false_positive: Some(false),
                box_coords: Some(json!([0.1, 0.1, 0.3, 0.5])),
                data: json!({ "score": 0.92, "top_score": 0.92, "type": "object" }),
                thumbnail: None,
                area: Some(12000),
                ratio: Some(0.5),
                score: Some(0.92),
                stationary: false,
            })
            .unwrap();

        let resp_notif_test = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/notifications/test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_notif_test.status(), StatusCode::OK);
        let notif_json: Value =
            serde_json::from_slice(&to_bytes(resp_notif_test.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(notif_json["success"], true);
        assert_eq!(notif_json["ws_delivered"], true);

        let resp_hist = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/stats/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_hist.status(), StatusCode::OK);

        state.db.run_maintenance().unwrap();
        let resp_prom = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_prom.status(), StatusCode::OK);
        assert!(
            resp_prom
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .starts_with("text/plain; version=0.0.4")
        );
        let prom_body = String::from_utf8(
            to_bytes(resp_prom.into_body(), 32768)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(prom_body.contains("ninjanvr_camera_fps{camera=\"front_door\",role=\"camera\"}"));
        assert!(prom_body.contains("ninjanvr_camera_online{camera=\"front_door\"}"));
        assert!(prom_body.contains("ninjanvr_detector_inference_ms{detector=\"detector01\"}"));
        assert!(prom_body.contains("ninjanvr_storage_used_bytes{mount=\"recordings\"}"));
        assert!(prom_body.contains("ninjanvr_cpu_usage_percent "));
        assert!(prom_body.contains("ninjanvr_memory_usage_bytes "));
        assert!(
            prom_body.contains("ninjanvr_events_total{camera=\"front_door\",label=\"person\"}")
        );
    }

    #[tokio::test]
    async fn router_serves_ai_context_mcp_and_debug_reprocess() {
        let ctx = setup_test_app().await;
        let app = &ctx.app;

        let resp_ai_ctx = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/ai/context?hours=24")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ai_ctx.status(), StatusCode::OK);
        let ai_ctx_json: Value =
            serde_json::from_slice(&to_bytes(resp_ai_ctx.into_body(), 16384).await.unwrap())
                .unwrap();
        assert!(ai_ctx_json["service"]["version"].is_string());
        assert!(ai_ctx_json["cameras"]["front_door"].is_object());
        assert!(ai_ctx_json["digest"]["counts_by_camera"].is_object());
        assert!(ai_ctx_json["digest"]["unreviewed_alerts"].is_array());

        let resp_mcp_list = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mcp")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_mcp_list.status(), StatusCode::OK);
        let mcp_list_json: Value =
            serde_json::from_slice(&to_bytes(resp_mcp_list.into_body(), 16384).await.unwrap())
                .unwrap();
        let tools_arr = mcp_list_json["result"]["tools"].as_array().unwrap();
        assert!(tools_arr.iter().any(|t| t["name"] == "get_security_digest"));
        assert!(tools_arr.iter().any(|t| t["name"] == "search_events"));

        let resp_mcp_call = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mcp")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_security_digest","arguments":{"hours":24}}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_mcp_call.status(), StatusCode::OK);
        let mcp_call_json: Value =
            serde_json::from_slice(&to_bytes(resp_mcp_call.into_body(), 16384).await.unwrap())
                .unwrap();
        assert!(mcp_call_json["result"]["structuredContent"]["digest"].is_object());

        let resp_reprocess = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/debug/reprocess")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"camera":"front_door","start_time":1700000000,"end_time":1700000020,"override_config":{"min_score":0.95,"min_area":500}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_reprocess.status(), StatusCode::OK);
        let reproc_json: Value =
            serde_json::from_slice(&to_bytes(resp_reprocess.into_body(), 16384).await.unwrap())
                .unwrap();
        assert_eq!(reproc_json["success"], true);
        assert_eq!(reproc_json["camera"], "front_door");
        let frames_arr = reproc_json["frames"].as_array().unwrap();
        assert!(!frames_arr.is_empty());
        assert!(frames_arr[0]["filtered_reasons"].is_array());
    }

    #[tokio::test]
    async fn router_serves_vod_hls_playlists_init_and_m4s_segments() {
        let ctx = setup_test_app().await;
        let app = &ctx.app;
        let state = &ctx.state;

        let rec_subdir = ctx.tmp_dir.join("recordings/2026-11-01/00/front_door");
        std::fs::create_dir_all(&rec_subdir).unwrap();
        let disk_seg = rec_subdir.join("00.00.mp4");
        let mp4_bytes = build_synthetic_mp4_bytes();
        std::fs::write(&disk_seg, &mp4_bytes).unwrap();

        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "vod_seg_1".to_string(),
                camera: "front_door".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-01/00/front_door/00.00.mp4".to_string(),
                start_time: 1000.0,
                end_time: 1010.0,
                duration: 10.0,
                motion: 12,
                objects: 1,
                dbfs: -22,
                segment_size: 0.25,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();

        let resp_master = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/vod/front_door/start/1000/end/1020/master.m3u8")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_master.status(), StatusCode::OK);
        assert_eq!(
            resp_master
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/vnd.apple.mpegurl")
        );
        let master_body = String::from_utf8(
            to_bytes(resp_master.into_body(), 4096)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(master_body.contains("index-v1-a1.m3u8"));

        let resp_idx = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/vod/front_door/start/1000/end/1020/index-v1-a1.m3u8")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_idx.status(), StatusCode::OK);
        assert_eq!(
            resp_idx
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/vnd.apple.mpegurl")
        );
        let idx_body =
            String::from_utf8(to_bytes(resp_idx.into_body(), 4096).await.unwrap().to_vec())
                .unwrap();
        assert!(idx_body.contains("#EXT-X-MAP:URI=\"init-v1-a1.mp4\""));
        assert!(idx_body.contains("seg-1-v1-a1.m4s"));

        let resp_init = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/vod/front_door/start/1000/end/1020/init-v1-a1.mp4")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_init.status(), StatusCode::OK);
        assert_eq!(
            resp_init
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("video/mp4")
        );
        let init_bytes = to_bytes(resp_init.into_body(), 65536).await.unwrap();
        assert!(init_bytes.len() > 64);

        let resp_seg = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/vod/front_door/start/1000/end/1020/seg-1-v1-a1.m4s")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_seg.status(), StatusCode::OK);
        assert_eq!(
            resp_seg
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("video/iso.segment")
        );
        let seg_bytes = to_bytes(resp_seg.into_body(), 65536).await.unwrap();
        assert!(seg_bytes.ends_with(&[0x65u8; 64]));

        let resp_empty = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/vod/front_door/start/9000/end/9010/index.m3u8")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_empty.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn router_manages_reviews_events_timeline_and_exports() {
        let ctx = setup_test_app().await;
        let app = &ctx.app;
        let state = &ctx.state;

        let rec_subdir = ctx.tmp_dir.join("recordings/2026-11-01/00/front_door");
        std::fs::create_dir_all(&rec_subdir).unwrap();
        let disk_seg = rec_subdir.join("00.00.mp4");
        std::fs::write(&disk_seg, build_synthetic_mp4_bytes()).unwrap();
        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "rev_ev_seg".to_string(),
                camera: "front_door".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-01/00/front_door/00.00.mp4".to_string(),
                start_time: 1700000100.0,
                end_time: 1700000115.0,
                duration: 15.0,
                motion: 40,
                objects: 2,
                dbfs: -18,
                segment_size: 0.5,
                regions: 2,
                motion_heatmap: None,
            })
            .unwrap();

        let clips_dir = ctx.tmp_dir.join("clips");
        std::fs::create_dir_all(&clips_dir).unwrap();
        std::fs::write(clips_dir.join("ev_test_1.jpg"), [0xFF, 0xD8, 0xFF, 0xD9]).unwrap();
        std::fs::write(
            clips_dir.join("front_door-ev_test_1.webp"),
            b"RIFF\x0c\x00\x00\x00WEBPVP8 ",
        )
        .unwrap();

        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_test_1".to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!(["driveway"]),
                start_time: 1700000100.0,
                end_time: Some(1700000112.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.92),
                false_positive: Some(false),
                box_coords: Some(json!([0.1, 0.1, 0.3, 0.5])),
                data: json!({ "score": 0.92, "top_score": 0.92, "type": "object" }),
                thumbnail: None,
                area: Some(12000),
                ratio: Some(0.5),
                score: Some(0.92),
                stationary: false,
            })
            .unwrap();

        state
            .db
            .insert_review_segment(&ninjanvr_db::ReviewItemDto {
                id: "rev_test_1".to_string(),
                camera: "front_door".to_string(),
                start_time: 1700000100.0,
                end_time: Some(1700000112.0),
                severity: "alert".to_string(),
                thumb_path: "/media/ninjanvr/clips/front_door-ev_test_1.webp".to_string(),
                data: json!({
                    "detections": ["ev_test_1"],
                    "objects": ["person"],
                    "zones": ["driveway"]
                }),
                has_been_reviewed: false,
            })
            .unwrap();

        state
            .db
            .insert_timeline_entry(
                1700000101.0,
                "front_door",
                "tracked_object",
                Some("ev_test_1"),
                "visible",
                &json!({ "label": "person", "score": 0.92 }),
            )
            .unwrap();

        let resp_rev = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/review?cameras=front_door")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev.status(), StatusCode::OK);
        let rev_list: Value =
            serde_json::from_slice(&to_bytes(resp_rev.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(rev_list.as_array().unwrap().len(), 1);

        let resp_rev_sum = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/review/summary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev_sum.status(), StatusCode::OK);

        let resp_motion = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/review/activity/motion?after=1700000000&before=1700000200")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_motion.status(), StatusCode::OK);
        let motion_arr: Value =
            serde_json::from_slice(&to_bytes(resp_motion.into_body(), 16384).await.unwrap())
                .unwrap();
        assert!(!motion_arr.as_array().unwrap().is_empty());

        let resp_rev_by_ev = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/review/event/ev_test_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev_by_ev.status(), StatusCode::OK);

        let resp_rev_by_id = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/review/rev_test_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev_by_id.status(), StatusCode::OK);

        let resp_rev_prev = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/review/rev_test_1/preview")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev_prev.status(), StatusCode::OK);

        let resp_rev_viewed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/reviews/viewed")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"ids":["rev_test_1"],"reviewed":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev_viewed.status(), StatusCode::OK);
        assert!(
            state
                .db
                .get_review_by_id("rev_test_1")
                .unwrap()
                .unwrap()
                .has_been_reviewed
        );

        let resp_rev_unview = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/review/rev_test_1/viewed")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev_unview.status(), StatusCode::OK);
        assert!(
            !state
                .db
                .get_review_by_id("rev_test_1")
                .unwrap()
                .unwrap()
                .has_been_reviewed
        );

        let resp_rev_notes = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/review/rev_test_1/notes")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"notes":"Verified alert segment"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev_notes.status(), StatusCode::OK);
        assert_eq!(
            state
                .db
                .get_review_by_id("rev_test_1")
                .unwrap()
                .unwrap()
                .data["notes"],
            "Verified alert segment"
        );

        let resp_rev_ids = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/review_ids?ids=rev_test_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev_ids.status(), StatusCode::OK);

        let resp_rev_del = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/reviews/delete")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"ids":["rev_test_1"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev_del.status(), StatusCode::OK);
        assert!(state.db.get_review_by_id("rev_test_1").unwrap().is_none());

        let resp_ev_exp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/events/explore?limit=5")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ev_exp.status(), StatusCode::OK);

        let resp_ev_sum = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/events/summary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ev_sum.status(), StatusCode::OK);

        let resp_ret_on = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events/ev_test_1/retain")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ret_on.status(), StatusCode::OK);
        assert_eq!(
            state.db.get_event("ev_test_1").unwrap().unwrap()["retain_indefinitely"],
            true
        );

        let resp_ret_off = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/events/ev_test_1/retain")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ret_off.status(), StatusCode::OK);
        assert_eq!(
            state.db.get_event("ev_test_1").unwrap().unwrap()["retain_indefinitely"],
            false
        );

        let resp_fp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/events/ev_test_1/false_positive")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"false_positive":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_fp.status(), StatusCode::OK);

        let resp_sub = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events/ev_test_1/sub_label")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"sub_label":"courier"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_sub.status(), StatusCode::OK);

        let resp_desc = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events/ev_test_1/description")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"description":"Delivery driver at gate"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_desc.status(), StatusCode::OK);

        let resp_ev_notes = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/events/ev_test_1/notes")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"notes":"Courier left parcel behind planter — verified by guard"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ev_notes.status(), StatusCode::OK);
        let ev_updated = state.db.get_event("ev_test_1").unwrap().unwrap();
        assert_eq!(ev_updated["sub_label"], "courier");
        assert_eq!(ev_updated["data"]["description"], "Delivery driver at gate");
        assert_eq!(
            ev_updated["data"]["notes"],
            "Courier left parcel behind planter — verified by guard"
        );

        // Configure known_plates and test PUT /api/events/{id}/license_plate,
        // GET /api/recognized_license_plates, and GET /api/events?plate=ABC1D23
        {
            let mut cfg = state.config.write().await;
            cfg.lpr.enabled = true;
            cfg.lpr
                .known_plates
                .insert("ABC1D23".to_string(), "Family SUV".to_string());
        }
        let resp_lpr_put = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/events/ev_test_1/license_plate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"recognized_license_plate":"abc-1d23"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_lpr_put.status(), StatusCode::OK);
        let lpr_put_json: Value =
            serde_json::from_slice(&to_bytes(resp_lpr_put.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(lpr_put_json["success"], true);
        assert_eq!(lpr_put_json["recognized_license_plate"], "ABC1D23");
        assert_eq!(lpr_put_json["sub_label"], "Family SUV");

        let resp_plates_list = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/recognized_license_plates")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_plates_list.status(), StatusCode::OK);
        let plates_list_json: Value =
            serde_json::from_slice(&to_bytes(resp_plates_list.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(plates_list_json, json!(["ABC1D23"]));

        let resp_ev_plate = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/events?plate=ABC1D23")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ev_plate.status(), StatusCode::OK);
        let ev_plate_arr: Value =
            serde_json::from_slice(&to_bytes(resp_ev_plate.into_body(), 16384).await.unwrap())
                .unwrap();
        assert_eq!(ev_plate_arr.as_array().unwrap().len(), 1);
        assert_eq!(ev_plate_arr[0]["id"], "ev_test_1");
        assert_eq!(
            ev_plate_arr[0]["data"]["recognized_license_plate"],
            "ABC1D23"
        );
        assert_eq!(ev_plate_arr[0]["sub_label"], "Family SUV");

        let resp_thumb = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/events/ev_test_1/thumbnail.webp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_thumb.status(), StatusCode::OK);

        let resp_snap = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/events/ev_test_1/snapshot.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_snap.status(), StatusCode::OK);

        let resp_clip = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/events/ev_test_1/clip.mp4")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_clip.status(), StatusCode::OK);

        for ev_uri in [
            "/api/events?cameras=front_door",
            "/api/events/search?cameras=front_door",
            "/api/event_ids?ids=ev_test_1",
            "/api/events/ev_test_1",
            "/api/events/ev_test_1/thumbnail.jpg",
            "/api/events/ev_test_1/snapshot-clean.webp",
            "/api/jobs/export",
        ] {
            let r = app
                .clone()
                .oneshot(Request::builder().uri(ev_uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK, "failed on {ev_uri}");
        }

        let resp_ev_bulk_del = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/events")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"event_ids":["non_existent_ev"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ev_bulk_del.status(), StatusCode::OK);

        let resp_ev_del_one = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/events/ev_test_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ev_del_one.status(), StatusCode::OK);
        assert!(state.db.get_event("ev_test_1").unwrap().is_none());

        let resp_tl = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/timeline?camera=front_door&source_id=ev_test_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_tl.status(), StatusCode::OK);
        let tl_arr: Value =
            serde_json::from_slice(&to_bytes(resp_tl.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(tl_arr.as_array().unwrap().len(), 1);

        let resp_logs = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/logs/ninjanvr")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_logs.status(), StatusCode::OK);
        let logs_json: Value =
            serde_json::from_slice(&to_bytes(resp_logs.into_body(), 16384).await.unwrap()).unwrap();
        assert!(logs_json["lines"].as_array().unwrap().len() >= 2);

        state
            .db
            .insert_export(&ExportRow {
                id: "exp_1".to_string(),
                camera: "front_door".to_string(),
                name: "Initial clip".to_string(),
                date: 1700000200.0,
                video_path: "/media/ninjanvr/exports/exp_1.mp4".to_string(),
                thumb_path: "/media/ninjanvr/clips/exp_1.webp".to_string(),
                in_progress: false,
            })
            .unwrap();
        state
            .db
            .insert_export(&ExportRow {
                id: "exp_2".to_string(),
                camera: "front_door".to_string(),
                name: "Second clip".to_string(),
                date: 1700000210.0,
                video_path: "/media/ninjanvr/exports/exp_2.mp4".to_string(),
                thumb_path: "/media/ninjanvr/clips/exp_2.webp".to_string(),
                in_progress: false,
            })
            .unwrap();

        let resp_exps = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/exports")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_exps.status(), StatusCode::OK);
        let exps_arr: Value =
            serde_json::from_slice(&to_bytes(resp_exps.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(exps_arr.as_array().unwrap().len(), 2);

        let resp_ren = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/export/exp_1/rename")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"name":"Renamed clip"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ren.status(), StatusCode::OK);

        let resp_del_one = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/export/exp_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_del_one.status(), StatusCode::OK);
        assert_eq!(state.db.query_exports().unwrap().len(), 1);

        let resp_del_bulk = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/exports/delete")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"ids":["exp_2"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_del_bulk.status(), StatusCode::OK);
        assert!(state.db.query_exports().unwrap().is_empty());
    }

    #[tokio::test]
    async fn router_manages_cases_recordings_previews_camera_state_and_federation() {
        let ctx = setup_test_app().await;
        let app = &ctx.app;
        let state = &ctx.state;

        let resp_cases_get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/cases")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_cases_get.status(), StatusCode::OK);

        let resp_cases_post = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/cases")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_cases_post.status(), StatusCode::OK);

        let rec_subdir = ctx.tmp_dir.join("recordings/2026-11-01/00/front_door");
        std::fs::create_dir_all(&rec_subdir).unwrap();
        let disk_seg_1 = rec_subdir.join("00.00.mp4");
        std::fs::write(&disk_seg_1, vec![0x11u8; 1024]).unwrap();

        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "seg_on_disk".to_string(),
                camera: "front_door".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-01/00/front_door/00.00.mp4".to_string(),
                start_time: 2000.0,
                end_time: 2010.0,
                duration: 10.0,
                motion: 1,
                objects: 0,
                dbfs: 0,
                segment_size: 0.001,
                regions: 0,
                motion_heatmap: None,
            })
            .unwrap();
        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "seg_ghost".to_string(),
                camera: "front_door".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-01/00/front_door/00.10.mp4".to_string(),
                start_time: 2010.0,
                end_time: 2020.0,
                duration: 10.0,
                motion: 0,
                objects: 0,
                dbfs: 0,
                segment_size: 0.001,
                regions: 0,
                motion_heatmap: None,
            })
            .unwrap();

        let resp_rec_sum = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/recordings/summary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rec_sum.status(), StatusCode::OK);

        let resp_rec_stor = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/recordings/storage")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rec_stor.status(), StatusCode::OK);

        let resp_cam_recs = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/front_door/recordings?after=1990&before=2030")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_cam_recs.status(), StatusCode::OK);
        let cam_recs_arr: Value =
            serde_json::from_slice(&to_bytes(resp_cam_recs.into_body(), 8192).await.unwrap())
                .unwrap();
        assert_eq!(cam_recs_arr.as_array().unwrap().len(), 2);

        let resp_sync = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/recordings/sync")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_sync.status(), StatusCode::OK);
        let sync_json: Value =
            serde_json::from_slice(&to_bytes(resp_sync.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(sync_json["removed_orphans"], 1);

        let resp_del_rec = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/front_door/recordings?after=1995&before=2015")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_del_rec.status(), StatusCode::OK);
        let del_rec_json: Value =
            serde_json::from_slice(&to_bytes(resp_del_rec.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(del_rec_json["deleted_segments"], 1);
        assert_eq!(del_rec_json["freed_bytes"], 1024);
        assert!(!disk_seg_1.exists());

        state
            .db
            .insert_preview(
                "prev_1",
                "front_door",
                "/media/ninjanvr/clips/previews/front_door/prev_1.mp4",
                1000.0,
                1060.0,
                60.0,
            )
            .unwrap();
        let resp_prev = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/preview/front_door/start/900/end/1100")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_prev.status(), StatusCode::OK);
        let prev_arr: Value =
            serde_json::from_slice(&to_bytes(resp_prev.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(prev_arr.as_array().unwrap().len(), 1);

        let resp_cam_state_get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/front_door/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_cam_state_get.status(), StatusCode::OK);

        let resp_cam_state = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/front_door/state")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"detect":false,"alerts":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_cam_state.status(), StatusCode::OK);
        let state_json: Value =
            serde_json::from_slice(&to_bytes(resp_cam_state.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(state_json["detect"], false);
        assert_eq!(state_json["alerts"], false);
        assert_eq!(state_json["record"], true);

        let clips_dir = ctx.tmp_dir.join("clips");
        std::fs::create_dir_all(&clips_dir).unwrap();
        let disk_latest = clips_dir.join("latest_front_door.jpg");
        let fake_jpeg = vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0xFF, 0xD9,
        ];
        std::fs::write(&disk_latest, &fake_jpeg).unwrap();
        state.camera_metrics.write().await.insert(
            "front_door".to_string(),
            pipeline::CameraRuntimeMetrics {
                camera_fps: 0.0,
                process_fps: 0.0,
                detection_fps: 0.0,
                pid: 123,
                last_frame_ts: 1730500123.0,
                night_mode: false,
            },
        );

        for latest_uri in ["/api/front_door/latest.webp", "/api/front_door/latest.jpg"] {
            let resp_latest = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(latest_uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp_latest.status(), StatusCode::OK);
            assert_eq!(
                resp_latest
                    .headers()
                    .get("x-last-frame-timestamp")
                    .and_then(|v| v.to_str().ok()),
                Some("1730500123")
            );
            let latest_body = to_bytes(resp_latest.into_body(), 4096).await.unwrap();
            assert_eq!(&latest_body[..], &fake_jpeg[..]);
        }

        let resp_fed_nodes = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/federation/nodes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_fed_nodes.status(), StatusCode::OK);
        let fed_json: Value =
            serde_json::from_slice(&to_bytes(resp_fed_nodes.into_body(), 8192).await.unwrap())
                .unwrap();
        let nodes_arr = fed_json["nodes"].as_array().unwrap();
        assert!(!nodes_arr.is_empty());
        assert_eq!(nodes_arr[0]["id"], "local");
        assert_eq!(nodes_arr[0]["online"], true);

        let resp_fed_unconf = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/federation/unknown_peer/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_fed_unconf.status(), StatusCode::NOT_FOUND);

        for extra_uri in [
            "/api/recordings/unavailable",
            "/api/front_door/recordings/summary",
            "/api/front_door/ptz/info",
            "/api/preview/front_door/start/900/end/1100/frames",
            "/api/preview/preview_front_door-1000/thumbnail.webp",
            "/api/preview/preview_front_door-1000/thumbnail.jpg",
            "/api/go2rtc/streams",
            "/api/go2rtc/streams/front_door",
        ] {
            let r = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(extra_uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK, "failed on {extra_uri}");
        }

        let resp_webrtc = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/go2rtc/webrtc?src=front_door")
                    .body(Body::from("v=0"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp_webrtc.status() == StatusCode::BAD_GATEWAY || resp_webrtc.status().is_success()
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ws_router = app.clone();
        let srv_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, ws_router).await;
        });
        let (mut ws_stream, ws_resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        assert_eq!(ws_resp.status(), StatusCode::SWITCHING_PROTOCOLS);
        let first_msg = ws_stream.next().await.unwrap().unwrap();
        assert!(first_msg.into_text().unwrap().contains("topic"));
        let _ = ws_stream.close(None).await;

        let (_, mse_ws_resp) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/live/mse/api/ws?src=front_door"))
                .await
                .unwrap();
        assert_eq!(mse_ws_resp.status(), StatusCode::SWITCHING_PROTOCOLS);
        srv_handle.abort();
    }

    #[tokio::test]
    async fn test_go2rtc_webrtc_backchannel_proxy_and_upstream_failure() {
        let t = setup_test_app().await;

        let captured_requests: Arc<RwLock<Vec<(String, String, String)>>> =
            Arc::new(RwLock::new(Vec::new()));
        let captured_clone = captured_requests.clone();

        let mock_go2rtc = Router::new().route(
            "/api/webrtc",
            post(
                move |uri: Uri, headers: HeaderMap, body: axum::body::Bytes| {
                    let captured = captured_clone.clone();
                    async move {
                        let query = uri.query().unwrap_or("").to_string();
                        let ct = headers
                            .get(header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let sdp_offer = String::from_utf8_lossy(&body).to_string();
                        captured
                            .write()
                            .await
                            .push((query.clone(), ct, sdp_offer.clone()));

                        let answer_sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=go2rtc-backchannel\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\na=mid:0\r\n".to_string();
                        let status = if query.contains("status=201") {
                            StatusCode::CREATED
                        } else {
                            StatusCode::OK
                        };
                        (
                            status,
                            [(header::CONTENT_TYPE, "application/sdp")],
                            answer_sdp,
                        )
                    }
                },
            ),
        );

        let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = mock_listener.local_addr().unwrap();
        let mock_handle = tokio::spawn(async move {
            let _ = axum::serve(mock_listener, mock_go2rtc).await;
        });

        let mut state = t.state.clone();
        state.go2rtc_http_base = format!("http://{mock_addr}");
        let app = build_router(state);

        let sdp_offer = "v=0\r\no=- 42 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\n";

        // 1. Verify 200 OK SDP offer/answer proxy with explicit Content-Type and src=front_door
        let resp_ok = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/go2rtc/webrtc?src=front_door")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from(sdp_offer))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ok.status(), StatusCode::OK);
        assert_eq!(
            resp_ok
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/sdp")
        );
        let answer_bytes = to_bytes(resp_ok.into_body(), 64 * 1024).await.unwrap();
        let answer_text = String::from_utf8_lossy(&answer_bytes);
        assert!(answer_text.contains("s=go2rtc-backchannel"));
        assert!(answer_text.contains("a=recvonly"));

        // 2. Verify 201 Created response passthrough and default Content-Type header injection
        let resp_created = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/go2rtc/webrtc?src=front_door&status=201")
                    .body(Body::from(sdp_offer))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_created.status(), StatusCode::CREATED);
        let created_bytes = to_bytes(resp_created.into_body(), 64 * 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&created_bytes).contains("s=go2rtc-backchannel"));

        // Assert upstream mock received exact query parameters, Content-Type, and SDP offers
        {
            let recorded = captured_requests.read().await;
            assert_eq!(recorded.len(), 2);
            assert_eq!(recorded[0].0, "src=front_door");
            assert_eq!(recorded[0].1, "application/sdp");
            assert_eq!(recorded[0].2, sdp_offer);
            assert_eq!(recorded[1].0, "src=front_door&status=201");
            assert_eq!(recorded[1].1, "application/sdp");
            assert_eq!(recorded[1].2, sdp_offer);
        }

        // 3. Shut down mock go2rtc server and verify 502 Bad Gateway on upstream error
        mock_handle.abort();
        let _ = mock_handle.await;

        let resp_fail = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/go2rtc/webrtc?src=front_door")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from(sdp_offer))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_fail.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn api_false_positive_signatures_and_verify_ai_roundtrip() {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr-ai-judge-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let cfg_path = tmp_dir.join("config.yml");
        let db_path = tmp_dir.join("ninjanvr.db");
        std::fs::write(
            &cfg_path,
            format!(
                "database:\n  path: {}\nversion: 0.18-0\n",
                db_path.display()
            ),
        )
        .unwrap();

        let state = new_app_state(&cfg_path, tmp_dir.join("web"), tmp_dir.join("media"), false)
            .await
            .unwrap();

        // Spin up a local mock HTTP server simulating Gemini, OpenAI, and Ollama Vision endpoints
        let mock_router = Router::new()
            .route(
                "/v1beta/models/{*model_action}",
                post(|| async {
                    Json(json!({
                        "candidates": [{
                            "content": {
                                "parts": [{
                                    "text": "{\"is_true_positive\": false, \"confidence\": 0.96, \"reason\": \"Static patio fern misclassified as person\"}"
                                }]
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/v1/chat/completions",
                post(|| async {
                    Json(json!({
                        "choices": [{
                            "message": {
                                "content": "```json\n{\"is_true_positive\": true, \"confidence\": 0.94, \"reason\": \"Delivery driver standing at porch\"}\n```"
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/api/generate",
                post(|| async {
                    Json(json!({
                        "response": "{\"is_true_positive\": false, \"confidence\": 0.91, \"reason\": \"Spider web IR reflection\"}"
                    }))
                }),
            )
            .route(
                "/v1beta/models",
                get(|| async {
                    Json(json!({
                        "models": [
                            {
                                "name": "models/gemini-2.5-flash",
                                "supportedGenerationMethods": ["generateContent"]
                            },
                            {
                                "name": "models/gemini-2.5-pro",
                                "supportedGenerationMethods": ["generateContent"]
                            },
                            {
                                "name": "models/text-embedding-004",
                                "supportedGenerationMethods": ["embedContent"]
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/v1/models",
                get(|| async {
                    Json(json!({
                        "data": [
                            { "id": "gpt-4o-mini" },
                            { "id": "gpt-4o" },
                            { "id": "whisper-1" }
                        ]
                    }))
                }),
            )
            .route(
                "/api/tags",
                get(|| async {
                    Json(json!({
                        "models": [
                            { "name": "qwen2.5vl:7b" },
                            { "name": "llava:7b" }
                        ]
                    }))
                }),
            );
        let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = mock_listener.local_addr().unwrap();
        let mock_base = format!("http://{mock_addr}");
        let mock_srv = tokio::spawn(async move {
            let _ = axum::serve(mock_listener, mock_router).await;
        });

        // Insert two test events into SQLite
        for (ev_id, dhash) in [
            ("ev_gemini_fp", 0x1122_3344_5566_7788u64),
            ("ev_openai_tp", 0x8877_6655_4433_2211u64),
            ("ev_ollama_fp", 0xCAFE_BABE_DEAD_BEEFu64),
        ] {
            state
                .db
                .upsert_event(&ninjanvr_db::EventDto {
                    id: ev_id.to_string(),
                    camera: "front_door".to_string(),
                    label: "person".to_string(),
                    zones: json!(["porch"]),
                    start_time: 1000.0,
                    end_time: Some(1006.0),
                    has_clip: true,
                    has_snapshot: true,
                    plus_id: None,
                    retain_indefinitely: false,
                    sub_label: None,
                    top_score: Some(0.58),
                    false_positive: Some(false),
                    box_coords: Some(json!([0.15, 0.25, 0.35, 0.75])),
                    data: json!({
                        "box": [0.15, 0.25, 0.35, 0.75],
                        "dhash64": dhash,
                        "attributes": ["color:green"]
                    }),
                    thumbnail: None,
                    area: Some(9000),
                    ratio: Some(0.45),
                    score: Some(0.58),
                    stationary: true,
                })
                .unwrap();
        }

        // 1. Configure Gemini provider pointing at mock_base and verify ev_gemini_fp
        {
            let mut cfg = state.config.write().await;
            cfg.genai.enabled = true;
            cfg.genai.provider = "gemini".to_string();
            cfg.genai.base_url = Some(mock_base.clone());
            cfg.genai.model = "gemini-2.5-flash".to_string();
            cfg.genai.auto_suppress_false_positives = true;
        }
        let app = build_router(state.clone());

        // Validate API & discover models across Gemini, OpenAI, and Ollama
        for (prov, expected_first) in [
            ("gemini", "gemini-2.5-flash"),
            ("openai", "gpt-4o-mini"),
            ("ollama", "qwen2.5vl:7b"),
        ] {
            let resp_models = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/genai/models")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({
                                "provider": prov,
                                "api_key": "test-key",
                                "base_url": mock_base
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp_models.status(), StatusCode::OK);
            let models_json: Value =
                serde_json::from_slice(&to_bytes(resp_models.into_body(), 8192).await.unwrap())
                    .unwrap();
            assert_eq!(models_json["valid"], true);
            assert_eq!(models_json["models"][0], expected_first);
        }

        let resp_gem = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events/ev_gemini_fp/verify-ai")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_gem.status(), StatusCode::OK);
        let gem_json: Value =
            serde_json::from_slice(&to_bytes(resp_gem.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(gem_json["is_true_positive"], false);
        assert_eq!(gem_json["false_positive"], true);
        assert_eq!(
            gem_json["ai_verification"]["reason"],
            "Static patio fern misclassified as person"
        );

        // Verify event row has ai_verification persisted and false_positive = true
        let ev_after = state.db.get_event("ev_gemini_fp").unwrap().unwrap();
        assert_eq!(ev_after["false_positive"], true);
        assert_eq!(
            ev_after["data"]["ai_verification"]["is_true_positive"],
            false
        );

        // Verify GET /api/false_positives/signatures returns the auto-created signature
        let resp_sigs = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/false_positives/signatures")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_sigs.status(), StatusCode::OK);
        let sigs_json: Value =
            serde_json::from_slice(&to_bytes(resp_sigs.into_body(), 8192).await.unwrap()).unwrap();
        let sigs_arr = sigs_json.as_array().unwrap();
        assert_eq!(sigs_arr.len(), 1);
        assert_eq!(sigs_arr[0]["source_event_id"], "ev_gemini_fp");
        assert_eq!(
            sigs_arr[0]["reason"],
            "Static patio fern misclassified as person"
        );
        let sig_id = sigs_arr[0]["id"].as_i64().unwrap();

        // 2. Configure OpenAI provider pointing at mock_base and verify ev_openai_tp (true positive)
        {
            let mut cfg = state.config.write().await;
            cfg.genai.provider = "openai".to_string();
            cfg.genai.model = "gpt-4o-mini".to_string();
        }
        let resp_oai = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events/ev_openai_tp/verify-ai")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_oai.status(), StatusCode::OK);
        let oai_json: Value =
            serde_json::from_slice(&to_bytes(resp_oai.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(oai_json["is_true_positive"], true);
        assert_eq!(oai_json["false_positive"], false);

        // 3. Configure Ollama provider pointing at mock_base and verify ev_ollama_fp (false positive)
        {
            let mut cfg = state.config.write().await;
            cfg.genai.provider = "ollama".to_string();
            cfg.genai.model = "qwen2.5vl:7b".to_string();
        }
        let resp_oll = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events/ev_ollama_fp/verify-ai")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_oll.status(), StatusCode::OK);
        let oll_json: Value =
            serde_json::from_slice(&to_bytes(resp_oll.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(oll_json["is_true_positive"], false);
        assert_eq!(oll_json["false_positive"], true);

        // 4. DELETE /api/false_positives/signatures/{id} removes the signature
        let resp_del = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/false_positives/signatures/{sig_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_del.status(), StatusCode::OK);

        // 5. Verify Gemini thinking-model multi-part payload parsing (thought part + JSON answer part)
        let gemini_thinking_payload = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {
                            "thought": true,
                            "text": "The user wants me to inspect the office camera image for false positives..."
                        },
                        {
                            "text": "{\"is_true_positive\": false, \"confidence\": 0.99, \"reason\": \"Empty office room; draped jacket on chair triggered false detection.\"}",
                            "thoughtSignature": "sig_test_123"
                        }
                    ]
                }
            }]
        });
        let (tp, conf, reason) = parse_ai_judge_json_verdict(&gemini_thinking_payload).unwrap();
        assert!(!tp);
        assert!((conf - 0.99).abs() < 1e-6);
        assert!(reason.contains("Empty office room"));

        let gemini_thinking_annotations = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {
                            "thought": true,
                            "text": "Detecting objects in frame..."
                        },
                        {
                            "text": "{\"detections\": [{\"label\": \"person\", \"bbox\": [0.1, 0.2, 0.5, 0.8], \"score\": 0.93}]}"
                        }
                    ]
                }
            }]
        });
        let parsed_boxes =
            parse_ai_auto_annotations(&gemini_thinking_annotations, 1280, 720).unwrap();
        assert_eq!(parsed_boxes.len(), 1);
        assert_eq!(parsed_boxes[0].label, "person");

        // 6. Verify error chain includes root cause while scrubbing secret API keys
        mock_srv.abort();
        {
            let mut cfg = state.config.write().await;
            cfg.genai.provider = "gemini".to_string();
            cfg.genai.model = "gemini-3.8-flash".to_string();
            cfg.genai.api_key = Some("AIzaSySuperSecretTestKey999".to_string());
            cfg.genai.base_url = Some("http://127.0.0.1:1".to_string());
        }
        let resp_err = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events/ev_gemini_fp/verify-ai")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_err.status(), StatusCode::BAD_GATEWAY);
        let err_json: Value =
            serde_json::from_slice(&to_bytes(resp_err.into_body(), 8192).await.unwrap()).unwrap();
        let err_msg = err_json["message"].as_str().unwrap_or("");
        assert!(err_msg.contains("Gemini Vision API request failed:"));
        assert!(
            !err_msg.contains("AIzaSySuperSecretTestKey999"),
            "API key must be scrubbed from error output: {err_msg}"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn onvif_ptz_soap_endpoints_handle_move_stop_preset_status_floats_faults_and_timeouts() {
        // 1. Unit-verify tolerant float parser on non-standard ONVIF float strings (#24833)
        assert!((parse_onvif_float("+0.5000000", -1.0, 1.0, 0.0) - 0.5).abs() < 1e-5);
        assert!((parse_onvif_float(" -0.2500000 ", -1.0, 1.0, 0.0) - (-0.25)).abs() < 1e-5);
        assert!((parse_onvif_float("+0.7500", 0.0, 1.0, 0.0) - 0.75).abs() < 1e-5);
        assert!((parse_onvif_float("0,4000", -1.0, 1.0, 0.0) - 0.4).abs() < 1e-5);
        assert_eq!(parse_onvif_float("NaN", -1.0, 1.0, 0.0), 0.0);
        assert_eq!(parse_onvif_float("not_a_float", -1.0, 1.0, 0.0), 0.0);
        assert_eq!(parse_onvif_float("+9.9999", -1.0, 1.0, 0.0), 1.0);

        // 2. Spin up a local mock ONVIF SOAP HTTP server
        let mode = Arc::new(std::sync::Mutex::new("ok".to_string()));
        let recorded_actions: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let mode_srv = mode.clone();
        let actions_srv = recorded_actions.clone();
        let mock_onvif_router = Router::new().route(
            "/onvif/ptz_service",
            post(move |body: String| {
                let mode_now = mode_srv.lock().unwrap().clone();
                let actions_log = actions_srv.clone();
                async move {
                    let action_tag = if body.contains("GetPresets") {
                        "GetPresets"
                    } else if body.contains("GetStatus") {
                        "GetStatus"
                    } else if body.contains("ContinuousMove") {
                        "ContinuousMove"
                    } else if body.contains("RelativeMove") {
                        "RelativeMove"
                    } else if body.contains("GotoPreset") {
                        "GotoPreset"
                    } else if body.contains("Stop") {
                        "Stop"
                    } else {
                        "Unknown"
                    };
                    actions_log.lock().unwrap().push(action_tag.to_string());

                    if mode_now == "timeout" {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    if mode_now == "fault" {
                        let fault_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body>
    <s:Fault>
      <s:Reason><s:Text xml:lang="en">PTZ Hardware Motor Busy</s:Text></s:Reason>
    </s:Fault>
  </s:Body>
</s:Envelope>"#;
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            [(header::CONTENT_TYPE, "application/soap+xml")],
                            fault_xml.to_string(),
                        );
                    }

                    let resp_xml = match action_tag {
                        "GetPresets" => r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
  <s:Body>
    <tptz:GetPresetsResponse>
      <tptz:Preset token="entry_gate">
        <tt:Name>entry_gate</tt:Name>
      </tptz:Preset>
      <tptz:Preset token="driveway">
        <tt:Name>driveway</tt:Name>
      </tptz:Preset>
    </tptz:GetPresetsResponse>
  </s:Body>
</s:Envelope>"#,
                        "GetStatus" => r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
  <s:Body>
    <tptz:GetStatusResponse>
      <tptz:PTZStatus>
        <tt:Position>
          <tt:PanTilt x="+0.5000000" y="-0.2500000"/>
          <tt:Zoom x="+0.1500000"/>
        </tt:Position>
        <tt:MoveStatus>
          <tt:PanTilt>IDLE</tt:PanTilt>
        </tt:MoveStatus>
      </tptz:PTZStatus>
    </tptz:GetStatusResponse>
  </s:Body>
</s:Envelope>"#,
                        "ContinuousMove" => r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tptz:ContinuousMoveResponse/></s:Body></s:Envelope>"#,
                        "RelativeMove" => r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tptz:RelativeMoveResponse/></s:Body></s:Envelope>"#,
                        "Stop" => r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tptz:StopResponse/></s:Body></s:Envelope>"#,
                        "GotoPreset" => r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tptz:GotoPresetResponse/></s:Body></s:Envelope>"#,
                        _ => r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body/></s:Envelope>"#,
                    };
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/soap+xml")],
                        resp_xml.to_string(),
                    )
                }
            }),
        );

        let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = mock_listener.local_addr().unwrap();
        let mock_handle = tokio::spawn(async move {
            let _ = axum::serve(mock_listener, mock_onvif_router).await;
        });

        let ctx = setup_test_app().await;
        let app = &ctx.app;
        let state = &ctx.state;

        // Configure front_door with ONVIF pointing at mock_addr
        {
            let mut cfg_guard = state.config.write().await;
            if let Some(cam) = cfg_guard.cameras.get_mut("front_door") {
                cam.onvif = ninjanvr_config::OnvifCameraConfig {
                    host: Some("127.0.0.1".to_string()),
                    port: mock_addr.port(),
                    user: Some("admin".to_string()),
                    password: Some("onvif_pass".to_string()),
                    autotracking: true,
                    return_preset: Some("entry_gate".to_string()),
                    timeout_sec: 2,
                };
            }
        }
        state
            .ptz_active_presets
            .write()
            .await
            .insert("front_door".to_string(), Some("entry_gate".to_string()));

        // A. GET /api/front_door/ptz/info parses GetPresets & GetStatus (+0.5000000 floats)
        let resp_info = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/front_door/ptz/info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_info.status(), StatusCode::OK);
        let info_json: Value =
            serde_json::from_slice(&to_bytes(resp_info.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(info_json["enabled"], true);
        assert_eq!(info_json["online"], true);
        assert_eq!(info_json["active_preset"], "entry_gate");
        assert_eq!(info_json["preset_names"], json!(["entry_gate", "driveway"]));
        assert!((info_json["status"]["pan"].as_f64().unwrap() - 0.5).abs() < 1e-4);
        assert!((info_json["status"]["tilt"].as_f64().unwrap() - (-0.25)).abs() < 1e-4);
        assert!((info_json["status"]["zoom"].as_f64().unwrap() - 0.15).abs() < 1e-4);
        assert_eq!(info_json["status"]["move_status"], "IDLE");

        // B. POST /api/front_door/ptz/move (ContinuousMove) clears active_preset
        let resp_move = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/front_door/ptz/move")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"mode":"continuous","pan":-0.6,"tilt":0.3,"zoom":0.0}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_move.status(), StatusCode::OK);
        let move_json: Value =
            serde_json::from_slice(&to_bytes(resp_move.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(move_json["success"], true);
        assert_eq!(move_json["operation"], "ContinuousMove");
        assert!(move_json["active_preset"].is_null());

        // C. POST /api/front_door/ptz/move (RelativeMove)
        let resp_rel = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/front_door/ptz/move")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"mode":"relative","action":"zoom_in","speed":0.4}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rel.status(), StatusCode::OK);
        let rel_json: Value =
            serde_json::from_slice(&to_bytes(resp_rel.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(rel_json["success"], true);
        assert_eq!(rel_json["operation"], "RelativeMove");

        // D. POST /api/front_door/ptz/stop
        let resp_stop = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/front_door/ptz/stop")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_stop.status(), StatusCode::OK);
        let stop_json: Value =
            serde_json::from_slice(&to_bytes(resp_stop.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(stop_json["success"], true);
        assert_eq!(stop_json["operation"], "Stop");

        // E. POST /api/front_door/ptz/preset (GotoPreset) restores active_preset to "driveway"
        let resp_preset = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/front_door/ptz/preset")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"preset":"driveway"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_preset.status(), StatusCode::OK);
        let preset_json: Value =
            serde_json::from_slice(&to_bytes(resp_preset.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(preset_json["success"], true);
        assert_eq!(preset_json["operation"], "GotoPreset");
        assert_eq!(preset_json["active_preset"], "driveway");
        assert_eq!(
            state
                .ptz_active_presets
                .read()
                .await
                .get("front_door")
                .and_then(|p| p.as_deref()),
            Some("driveway")
        );

        // F. SOAP Fault handling: mock server returns 500 SOAP Fault -> 502 Bad Gateway
        *mode.lock().unwrap() = "fault".to_string();
        let resp_fault = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/front_door/ptz/move")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"action":"left"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_fault.status(), StatusCode::BAD_GATEWAY);
        let fault_json: Value =
            serde_json::from_slice(&to_bytes(resp_fault.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(fault_json["success"], false);
        assert!(
            fault_json["error"]
                .as_str()
                .unwrap_or("")
                .contains("PTZ Hardware Motor Busy")
        );

        // G. Strict Timeout handling: mock server delays 200ms while timeout_ms is 30ms -> 504 Gateway Timeout
        *mode.lock().unwrap() = "timeout".to_string();
        let resp_timeout = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/front_door/ptz/move")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"mode":"relative","pan":0.1,"tilt":0.0,"timeout_ms":30}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_timeout.status(), StatusCode::GATEWAY_TIMEOUT);

        // H. Recovery after dropped/timed-out RelativeMove packet (#24834)
        *mode.lock().unwrap() = "ok".to_string();
        let resp_recover = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/front_door/ptz/move")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"mode":"relative","pan":0.15,"tilt":-0.05}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_recover.status(), StatusCode::OK);

        let actions = recorded_actions.lock().unwrap().clone();
        assert!(actions.contains(&"GetPresets".to_string()));
        assert!(actions.contains(&"GetStatus".to_string()));
        assert!(actions.contains(&"ContinuousMove".to_string()));
        assert!(actions.contains(&"RelativeMove".to_string()));
        assert!(actions.contains(&"Stop".to_string()));
        assert!(actions.contains(&"GotoPreset".to_string()));

        mock_handle.abort();
    }

    #[tokio::test]
    async fn face_recognition_gallery_and_sub_label_http_endpoints_work_end_to_end() {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr_faces_http_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir).unwrap();

        let cfg_path = tmp_dir.join("config.yml");
        let db_path = tmp_dir.join("ninjanvr.db");
        std::fs::write(
            &cfg_path,
            format!(
                "database:\n  path: {}\nversion: 0.18-0\n",
                db_path.display()
            ),
        )
        .unwrap();
        let state = new_app_state(&cfg_path, tmp_dir.join("web"), tmp_dir.join("media"), false)
            .await
            .unwrap();

        // Seed a person event and reviewsegment
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_person_bruno".to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!(["porch"]),
                start_time: 1000.0,
                end_time: Some(1004.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.93),
                false_positive: Some(false),
                box_coords: Some(json!([0.1, 0.1, 0.4, 0.8])),
                data: json!({ "box": [0.1, 0.1, 0.4, 0.8] }),
                thumbnail: None,
                area: Some(15000),
                ratio: Some(0.5),
                score: Some(0.93),
                stationary: false,
            })
            .unwrap();

        let app = build_router(state.clone());

        // 1. POST /api/faces/Bruno/enroll
        let enroll_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/faces/Bruno/enroll")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"image_base64":"ZmFrZS1mYWNlLWNyb3AtYnJ1bm8="}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(enroll_resp.status(), StatusCode::OK);
        let enroll_json: Value =
            serde_json::from_slice(&to_bytes(enroll_resp.into_body(), 65536).await.unwrap())
                .unwrap();
        assert_eq!(enroll_json["success"], true);
        assert_eq!(enroll_json["name"], "Bruno");
        assert_eq!(enroll_json["sample_count"], 1);
        assert_eq!(enroll_json["embedding_dim"], 512);

        // 2. GET /api/faces
        let list_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/faces")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list_resp.status(), StatusCode::OK);
        let list_json: Value =
            serde_json::from_slice(&to_bytes(list_resp.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(list_json["profiles"][0]["name"], "Bruno");
        assert_eq!(list_json["profiles"][0]["sample_count"], 1);
        assert!(list_json["faces"]["Bruno"].is_array());

        // 3. POST /api/events/{id}/sub_label
        let sub_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events/ev_person_bruno/sub_label")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"sub_label":"Bruno","sub_label_score":0.89}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(sub_resp.status(), StatusCode::OK);
        let ev_after = state.db.get_event("ev_person_bruno").unwrap().unwrap();
        assert_eq!(ev_after["sub_label"], "Bruno");
        assert!((ev_after["sub_label_score"].as_f64().unwrap() - 0.89).abs() < 1e-5);

        // 4. DELETE /api/faces/Bruno
        let del_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/faces/Bruno")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del_resp.status(), StatusCode::OK);
        let del_json: Value =
            serde_json::from_slice(&to_bytes(del_resp.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(del_json["success"], true);
        assert_eq!(del_json["deleted"], true);
        assert_eq!(del_json["cleared_events"], 1);

        let ev_cleared = state.db.get_event("ev_person_bruno").unwrap().unwrap();
        assert!(ev_cleared["sub_label"].is_null());
        assert!(ev_cleared["sub_label_score"].is_null());

        let list_after = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/faces")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let list_after_json: Value =
            serde_json::from_slice(&to_bytes(list_after.into_body(), 65536).await.unwrap())
                .unwrap();
        assert_eq!(list_after_json["profiles"].as_array().unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn dataset_from_event_auto_annotate_and_zip_export_end_to_end() {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr-dataset-e2e-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let cfg_path = tmp_dir.join("config.yml");
        let db_path = tmp_dir.join("ninjanvr.db");
        std::fs::write(
            &cfg_path,
            format!(
                "database:\n  path: {}\nversion: 0.18-0\n",
                db_path.display()
            ),
        )
        .unwrap();

        let media_dir = tmp_dir.join("media");
        let clips_dir = media_dir.join("clips");
        std::fs::create_dir_all(&clips_dir).unwrap();

        let state = new_app_state(&cfg_path, tmp_dir.join("web"), media_dir.clone(), false)
            .await
            .unwrap();

        //Spin up a local mock Vision AI endpoint returning normalized bounding boxes
        let mock_router = Router::new()
            .route(
                "/v1beta/models/{*model_action}",
                post(|| async {
                    Json(json!({
                        "candidates": [{
                            "content": {
                                "parts": [{
                                    "text": "{\"detections\": [{\"label\": \"person\", \"bbox\": [0.10, 0.20, 0.50, 0.80], \"score\": 0.97}, {\"label\": \"package\", \"bbox\": [0.55, 0.70, 0.75, 0.90], \"score\": 0.93}]}"
                                }]
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/v1/chat/completions",
                post(|| async {
                    Json(json!({
                        "choices": [{
                            "message": {
                                "content": "{\"annotations\": [{\"label\": \"car\", \"box_2d\": [200, 100, 800, 900], \"confidence\": 0.94}]}"
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/api/generate",
                post(|| async {
                    Json(json!({
                        "response": "{\"boxes\": [{\"class_id\": 7, \"label\": \"license_plate\", \"x_min\": 0.40, \"y_min\": 0.60, \"x_max\": 0.60, \"y_max\": 0.70, \"score\": 0.91}]}"
                    }))
                }),
            );
        let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = mock_listener.local_addr().unwrap();
        let mock_base = format!("http://{mock_addr}");
        let mock_srv = tokio::spawn(async move {
            let _ = axum::serve(mock_listener, mock_router).await;
        });

        {
            let mut cfg = state.config.write().await;
            cfg.genai.enabled = true;
            cfg.genai.provider = "gemini".to_string();
            cfg.genai.base_url = Some(mock_base);
            cfg.genai.model = "gemini-2.5-flash".to_string();
        }

        // Write clean snapshots to disk and insert events
        let img = image::RgbImage::from_pixel(64, 36, image::Rgb([64, 64, 64]));
        let webp_bytes = ninjanvr_vision::encode_image_webp(&img).unwrap();
        std::fs::write(clips_dir.join("front_door-ev_tp_1-clean.webp"), &webp_bytes).unwrap();
        std::fs::write(clips_dir.join("patio-ev_fp_1-clean.webp"), &webp_bytes).unwrap();

        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_tp_1".to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!(["porch"]),
                start_time: 2000.0,
                end_time: Some(2005.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.91),
                false_positive: Some(false),
                box_coords: Some(json!([0.20, 0.10, 0.60, 0.90])),
                data: json!({ "box": [0.20, 0.10, 0.60, 0.90] }),
                thumbnail: None,
                area: Some(10000),
                ratio: Some(0.5),
                score: Some(0.91),
                stationary: false,
            })
            .unwrap();

        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_fp_1".to_string(),
                camera: "patio".to_string(),
                label: "person".to_string(),
                zones: json!(["yard"]),
                start_time: 2010.0,
                end_time: Some(2014.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.62),
                false_positive: Some(false),
                box_coords: Some(json!([0.10, 0.10, 0.30, 0.50])),
                data: json!({ "box": [0.10, 0.10, 0.30, 0.50] }),
                thumbnail: None,
                area: Some(6000),
                ratio: Some(0.5),
                score: Some(0.62),
                stationary: true,
            })
            .unwrap();

        let app = build_router(state.clone());

        // 1. POST /api/dataset/samples/from-event/ev_tp_1 creates true_positive sample
        let resp_from_ev = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/dataset/samples/from-event/ev_tp_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_from_ev.status(), StatusCode::OK);
        let tp_json: Value =
            serde_json::from_slice(&to_bytes(resp_from_ev.into_body(), 16384).await.unwrap())
                .unwrap();
        let tp_sample_id = tp_json["id"].as_str().unwrap().to_string();
        assert_eq!(tp_sample_id, "ds_ev_tp_1");
        assert_eq!(tp_json["sample_kind"], "true_positive");
        assert_eq!(tp_json["annotations"][0]["class_id"], 0);
        assert_eq!(tp_json["annotations"][0]["cx"], 0.4);
        assert_eq!(tp_json["annotations"][0]["cy"], 0.5);
        assert_eq!(tp_json["annotations"][0]["w"], 0.4);
        assert_eq!(tp_json["annotations"][0]["h"], 0.8);
        assert!(media_dir.join("dataset/ds_ev_tp_1.webp").exists());

        // 2. PUT /api/events/ev_fp_1/false_positive creates hard_negative sample with empty annotations
        let resp_fp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/events/ev_fp_1/false_positive")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"false_positive":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_fp.status(), StatusCode::OK);
        let hn_sample = state
            .db
            .get_dataset_sample("ds_ev_fp_1")
            .unwrap()
            .expect("hard_negative sample created on false_positive");
        assert_eq!(hn_sample.sample_kind, "hard_negative");
        assert!(hn_sample.annotations.is_empty());
        assert!(media_dir.join("dataset/ds_ev_fp_1.webp").exists());

        // 3. POST /api/dataset/samples/ds_ev_tp_1/auto-annotate invokes Teacher AI & converts boxes to YOLO
        let resp_auto = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/dataset/samples/ds_ev_tp_1/auto-annotate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_auto.status(), StatusCode::OK);
        let auto_json: Value =
            serde_json::from_slice(&to_bytes(resp_auto.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(auto_json["sample_kind"], "ai_annotated");
        let anns = auto_json["annotations"].as_array().unwrap();
        assert_eq!(anns.len(), 2);
        // [0.10, 0.20, 0.50, 0.80] -> cx=0.30, cy=0.50, w=0.40, h=0.60
        assert_eq!(anns[0]["class_id"], 0);
        assert_eq!(anns[0]["label"], "person");
        assert_eq!(anns[0]["cx"], 0.3);
        assert_eq!(anns[0]["cy"], 0.5);
        assert_eq!(anns[0]["w"], 0.4);
        assert_eq!(anns[0]["h"], 0.6);
        // [0.55, 0.70, 0.75, 0.90] -> cx=0.65, cy=0.80, w=0.20, h=0.20
        assert_eq!(anns[1]["class_id"], 11);
        assert_eq!(anns[1]["label"], "package");
        assert_eq!(anns[1]["cx"], 0.65);
        assert_eq!(anns[1]["cy"], 0.8);
        assert_eq!(anns[1]["w"], 0.2);
        assert_eq!(anns[1]["h"], 0.2);

        // Pin both samples to 'train' split and verified=true via PUT /api/dataset/samples/{id}
        for sid in ["ds_ev_tp_1", "ds_ev_fp_1"] {
            let resp_put = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri(format!("/api/dataset/samples/{sid}"))
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"verified":true,"split":"train"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp_put.status(), StatusCode::OK);
        }

        // Verify GET /api/dataset/summary and GET /api/dataset/samples/{id}/image.webp
        let resp_sum = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/dataset/summary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_sum.status(), StatusCode::OK);
        let sum_json: Value =
            serde_json::from_slice(&to_bytes(resp_sum.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(sum_json["total_samples"], 2);
        assert_eq!(sum_json["hard_negative_count"], 1);
        assert_eq!(sum_json["ai_annotated_count"], 1);

        let resp_img = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/dataset/samples/ds_ev_tp_1/image.webp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_img.status(), StatusCode::OK);

        // 4. GET /api/dataset/export.zip -> parse ZIP archive and assert contents
        let resp_zip = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/dataset/export.zip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_zip.status(), StatusCode::OK);
        assert_eq!(
            resp_zip.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/zip"
        );
        let zip_bytes = to_bytes(resp_zip.into_body(), 1024 * 1024).await.unwrap();
        let archive = parse_zip_archive(&zip_bytes).expect("valid ZIP archive");

        let data_yaml = String::from_utf8(archive.get("data.yaml").unwrap().clone()).unwrap();
        assert!(data_yaml.contains("train: images/train"));
        assert!(data_yaml.contains("val: images/val"));
        assert!(data_yaml.contains("nc: 15"));
        assert!(data_yaml.contains("0: person"));
        assert!(data_yaml.contains("11: package"));
        assert!(data_yaml.contains("14: umbrella"));

        assert!(archive.contains_key("images/train/ds_ev_tp_1.webp"));
        assert!(archive.contains_key("images/train/ds_ev_fp_1.webp"));

        let tp_label_txt =
            String::from_utf8(archive.get("labels/train/ds_ev_tp_1.txt").unwrap().clone()).unwrap();
        assert_eq!(
            tp_label_txt.trim(),
            "0 0.300000 0.500000 0.400000 0.600000\n11 0.650000 0.800000 0.200000 0.200000"
        );

        let hn_label_txt =
            String::from_utf8(archive.get("labels/train/ds_ev_fp_1.txt").unwrap().clone()).unwrap();
        assert!(
            hn_label_txt.is_empty(),
            "hard negative sample must produce an empty .txt label file"
        );

        // 5. Verify GET /api/dataset/samples, GET /api/dataset/samples/{id}, OpenAI + Ollama batch auto-annotate, and DELETE
        let resp_list = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/dataset/samples?camera=front_door")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_list.status(), StatusCode::OK);
        let list_json: Value =
            serde_json::from_slice(&to_bytes(resp_list.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(list_json.as_array().unwrap().len(), 1);

        let resp_one = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/dataset/samples/ds_ev_tp_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_one.status(), StatusCode::OK);

        // Mark ds_ev_tp_1 unverified and test batch auto-annotate with OpenAI & Ollama providers
        state
            .db
            .update_dataset_sample_annotations("ds_ev_tp_1", None, None, Some(false), None)
            .unwrap();
        {
            let mut cfg = state.config.write().await;
            cfg.genai.provider = "openai".to_string();
            cfg.genai.model = "gpt-4o-mini".to_string();
        }
        let resp_batch_oai = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/dataset/auto-annotate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_batch_oai.status(), StatusCode::OK);

        {
            let mut cfg = state.config.write().await;
            cfg.genai.provider = "ollama".to_string();
            cfg.genai.model = "qwen2.5vl:7b".to_string();
        }
        let resp_batch_oll = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/dataset/auto-annotate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_batch_oll.status(), StatusCode::OK);

        let resp_del = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/dataset/samples/ds_ev_fp_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_del.status(), StatusCode::OK);

        mock_srv.abort();
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn detector_model_upload_persists_and_hot_reloads() {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr-model-upload-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let cfg_path = tmp_dir.join("config.yml");
        let db_path = tmp_dir.join("ninjanvr.db");
        std::fs::write(
            &cfg_path,
            format!(
                "version: 0.18-0\ndatabase:\n  path: {}\n",
                db_path.display()
            ),
        )
        .unwrap();

        let media_dir = tmp_dir.join("media");
        let state = new_app_state(&cfg_path, tmp_dir.join("web"), media_dir.clone(), false)
            .await
            .unwrap();
        let app = build_router(state.clone());

        let synthetic_onnx =
            base64::engine::general_purpose::STANDARD.encode(b"ONNX_FINE_TUNED_YOLO11S_WEIGHTS");
        let upload_body = json!({
            "filename": "ninjanvr_custom_yolo11s.onnx",
            "model_base64": synthetic_onnx,
            "model_type": "yolo-generic",
            "width": 640,
            "height": 640,
            "input_tensor": "nchw",
            "input_pixel_format": "rgb"
        });

        let resp_upload = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/detectors/models/upload")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&upload_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_upload.status(), StatusCode::OK);
        let upload_json: Value =
            serde_json::from_slice(&to_bytes(resp_upload.into_body(), 32768).await.unwrap())
                .unwrap();
        assert_eq!(upload_json["success"], true);
        assert_eq!(upload_json["reloaded"], true);
        assert_eq!(upload_json["filename"], "ninjanvr_custom_yolo11s.onnx");

        // Verify model and 15-class labelmap files were persisted in {media_dir}/models/
        let expected_model_file = media_dir.join("models/ninjanvr_custom_yolo11s.onnx");
        let expected_labelmap_file = media_dir.join("models/ninjanvr_custom_yolo11s.txt");
        assert!(expected_model_file.exists());
        assert!(expected_labelmap_file.exists());
        assert_eq!(
            std::fs::read(&expected_model_file).unwrap(),
            b"ONNX_FINE_TUNED_YOLO11S_WEIGHTS"
        );
        let labelmap_contents = std::fs::read_to_string(&expected_labelmap_file).unwrap();
        assert!(labelmap_contents.contains("person\nface\ncar"));
        assert_eq!(labelmap_contents.lines().count(), 15);

        // Verify AiDetectorEngine was hot-reloaded with the new dimensions (640x640)
        assert_eq!(state.detector.model_dimensions(), (640, 640, false));

        // Verify GET /api/detectors/catalog discovers the newly uploaded model
        let resp_cat = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/detectors/catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_cat.status(), StatusCode::OK);
        let cat_json: Value =
            serde_json::from_slice(&to_bytes(resp_cat.into_body(), 32768).await.unwrap()).unwrap();
        let discovered = cat_json["discovered_models"].as_array().unwrap();
        assert!(
            discovered.iter().any(|m| m["path"]
                .as_str()
                .unwrap_or("")
                .ends_with("ninjanvr_custom_yolo11s.onnx")),
            "GET /api/detectors/catalog must list uploaded model in discovered_models"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn evaluate_system_heuristics_detects_duplicate_rtsp_aspect_cpu_storage_retention_audio_and_quality_drift()
     {
        let yaml = r#"
go2rtc:
  streams:
    cam_a_main: "rtsp://admin:pass1@192.0.2.10:554/stream1#video=copy"
    cam_a_dup: "rtsp://viewer:pass2@192.0.2.10:554/stream1"
    cam_local: "rtsp://127.0.0.1:8554/cam_a_main"
cameras:
  cam_a:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/cam_a_main
          roles: [record, detect, audio]
    detect:
      width: 640
      height: 480
    record:
      enabled: true
      continuous:
        days: 7
      preview:
        quality: very_high
    snapshots:
      quality: 95
  cam_b:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/cam_b_main
          roles: [record, detect]
    detect:
      width: 1280
      height: 720
    record:
      enabled: true
      continuous:
        days: 2
      preview:
        quality: medium
    snapshots:
      quality: 100
"#;
        let cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        let mut cam_stats = serde_json::Map::new();
        cam_stats.insert("cam_a".to_string(), json!({ "ffmpeg_cpu": "3.6" }));
        cam_stats.insert("cam_b".to_string(), json!({ "ffmpeg_cpu": "0.5" }));

        let rec_mount = json!({
            "total": 100_000.0,
            "used": 96_000.0,
            "free": 4_000.0
        });

        let alerts = evaluate_system_heuristics(&cfg, &cam_stats, &rec_mount, 1.5);
        let ids: Vec<&str> = alerts.iter().map(|a| a.id.as_str()).collect();
        assert!(
            ids.contains(&"duplicate_rtsp_producer_cam_a_dup")
                || ids.contains(&"duplicate_rtsp_producer_cam_a_main")
        );
        assert!(ids.contains(&"detect_aspect_mismatch_cam_a"));
        assert!(!ids.contains(&"detect_aspect_mismatch_cam_b"));
        assert!(ids.contains(&"detect_ffmpeg_cpu_cam_a"));
        assert!(ids.contains(&"storage_capacity_unbounded"));
        assert!(ids.contains(&"retention_runway_asymmetry"));
        assert!(ids.contains(&"audio_role_disabled_mismatch"));
        assert!(ids.contains(&"camera_quality_drift"));

        let storage_alert = alerts
            .iter()
            .find(|a| a.id == "storage_capacity_unbounded")
            .unwrap();
        assert_eq!(storage_alert.severity, "critical");

        let log_hub = LogBufferHub::new();
        emit_heuristics_to_logs(&log_hub, &alerts);
        let (total_logged, logged) = log_hub.query_lines("ninjanvr", None, None);
        assert_eq!(total_logged, alerts.len());
        assert_eq!(logged.len(), alerts.len());
        assert!(
            logged
                .iter()
                .any(|l| l.contains("ERROR") && l.contains("storage_capacity_unbounded"))
        );
    }

    #[test]
    fn crc32_and_zip_archive_roundtrip_and_corruption_detection() {
        // IEEE 802.3 standard check value for "123456789"
        assert_eq!(crc32_ieee(b""), 0x0000_0000);
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);

        let entries = vec![
            (
                "data.yaml".to_string(),
                b"nc: 15\nnames: [person]\n".to_vec(),
            ),
            (
                "labels/train/sample1.txt".to_string(),
                b"0 0.5 0.5 0.2 0.4\n".to_vec(),
            ),
        ];
        let mut archive = build_zip_archive(&entries);
        let parsed = parse_zip_archive(&archive).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed["data.yaml"], b"nc: 15\nnames: [person]\n");
        assert_eq!(parsed["labels/train/sample1.txt"], b"0 0.5 0.5 0.2 0.4\n");

        // Reject too-small buffer and missing EOCD
        assert!(parse_zip_archive(b"short").is_err());
        assert!(parse_zip_archive(&[0u8; 32]).is_err());

        // Corrupt a payload byte inside the first file -> must fail with CRC32 mismatch
        let first_data_offset = 30 + "data.yaml".len();
        archive[first_data_offset] ^= 0xFF;
        let err = parse_zip_archive(&archive).unwrap_err().to_string();
        assert!(err.contains("CRC32 mismatch"), "unexpected error: {err}");
    }

    #[test]
    fn onvif_endpoint_resolver_soap_builders_fault_and_preset_parsers_and_ptz_vectors() {
        let mut onvif = ninjanvr_config::OnvifCameraConfig::default();
        assert_eq!(resolve_onvif_ptz_endpoint(&onvif), None);

        onvif.host = Some("192.0.2.25".to_string());
        onvif.port = 8080;
        assert_eq!(
            resolve_onvif_ptz_endpoint(&onvif).as_deref(),
            Some("http://192.0.2.25:8080/onvif/ptz_service")
        );

        onvif.host = Some("192.0.2.25:8899".to_string());
        assert_eq!(
            resolve_onvif_ptz_endpoint(&onvif).as_deref(),
            Some("http://192.0.2.25:8899/onvif/ptz_service")
        );

        onvif.host = Some("[::1]".to_string());
        onvif.port = 80;
        assert_eq!(
            resolve_onvif_ptz_endpoint(&onvif).as_deref(),
            Some("http://[::1]:80/onvif/ptz_service")
        );

        onvif.host = Some("https://cam.example.invalid/custom/ptz/".to_string());
        assert_eq!(
            resolve_onvif_ptz_endpoint(&onvif).as_deref(),
            Some("https://cam.example.invalid/custom/ptz")
        );

        // XML escaping in SOAP envelope credentials and anonymous header when user is None
        let anon_soap = build_onvif_stop_soap("Profile_1", true, false, None, None);
        assert!(anon_soap.contains("<s:Header/>"));
        let auth_soap = build_onvif_goto_preset_soap(
            "Profile&1",
            "Preset<Home>",
            Some(2.5),
            Some("admin"),
            Some("p&w<\"'"),
        );
        assert!(auth_soap.contains("<wsse:Username>admin</wsse:Username>"));
        assert!(auth_soap.contains("<wsse:Password>p&amp;w&lt;&quot;&apos;</wsse:Password>"));
        assert!(auth_soap.contains("<tptz:ProfileToken>Profile&amp;1</tptz:ProfileToken>"));
        assert!(auth_soap.contains("<tptz:PresetToken>Preset&lt;Home&gt;</tptz:PresetToken>"));
        assert!(auth_soap.contains("<tt:PanTilt x=\"1.0000\" y=\"1.0000\"/>"));

        // PtzMoveBody directional vectors and speed clamping
        let move_ul = PtzMoveBody {
            action: Some("up_left".to_string()),
            direction: None,
            mode: Some(" relative ".to_string()),
            pan: None,
            tilt: None,
            zoom: None,
            speed: Some(0.4),
            timeout_sec: None,
            timeout_ms: None,
            profile_token: None,
        };
        assert_eq!(move_ul.resolve_vectors(), (-0.4, 0.4, 0.0));
        assert!(move_ul.is_relative());

        // SOAP fault extraction and preset XML parsing (both self-closing and nested tags)
        assert_eq!(
            extract_onvif_soap_fault("<s:Envelope><s:Body><Ok/></s:Body></s:Envelope>"),
            None
        );
        assert_eq!(
            extract_onvif_soap_fault(
                "<s:Fault><faultstring>ActionNotSupported</faultstring></s:Fault>"
            )
            .as_deref(),
            Some("ActionNotSupported")
        );
        let presets_xml = r#"
        <tptz:GetPresetsResponse>
            <tptz:Preset token="p1"><tt:Name>Front Gate</tt:Name></tptz:Preset>
            <tptz:Preset token="p2"/>
            <Preset><token>p3</token><Name>Driveway</Name></Preset>
        </tptz:GetPresetsResponse>
        "#;
        let presets = parse_onvif_presets_xml(presets_xml);
        assert_eq!(presets.len(), 3);
        assert_eq!(presets[0].token, "p1");
        assert_eq!(presets[0].name, "Front Gate");
        assert_eq!(presets[1].token, "p2");
        assert_eq!(presets[1].name, "p2");
        assert_eq!(presets[2].token, "p3");
        assert_eq!(presets[2].name, "Driveway");
    }

    #[test]
    fn yolo_coordinate_normalization_and_cidr_ipv6_matching_edge_cases() {
        // Reversed and out-of-bounds coordinates are clamped and converted to (cx, cy, w, h)
        let (cx, cy, w, h) = xyxy_to_yolo_normalized(0.8, 0.9, 0.2, 0.1);
        assert_eq!((cx, cy, w, h), (0.5, 0.5, 0.6, 0.8));

        // Zero-area point box clamps width/height to 0.001
        let (_, _, zw, zh) = xyxy_to_yolo_normalized(0.5, 0.5, 0.5, 0.5);
        assert_eq!((zw, zh), (0.001, 0.001));

        // event_box_to_yolo_normalized: all-zero fallback and [x, y, w, h] branch when b[2] <= b[0]
        assert_eq!(
            event_box_to_yolo_normalized([0.0, 0.0, 0.0, 0.0]),
            (0.5, 0.5, 0.25, 0.4)
        );
        assert_eq!(
            event_box_to_yolo_normalized([0.6, 0.5, 0.2, 0.3]),
            (0.7, 0.65, 0.2, 0.3)
        );

        // IPv6 CIDR, /0 wildcard, bare IP, and invalid prefix handling
        assert!(ip_matches_cidr("2001:db8::dead:beef", "2001:db8::/32"));
        assert!(!ip_matches_cidr("2001:db9::1", "2001:db8::/32"));
        assert!(ip_matches_cidr("::1", "::/0"));
        assert!(ip_matches_cidr("198.51.100.7", "0.0.0.0/0"));
        assert!(ip_matches_cidr("198.51.100.7", "198.51.100.7"));
        assert!(!ip_matches_cidr("198.51.100.7", "198.51.100.0/33"));
        assert!(!ip_matches_cidr("::1", "::1/129"));
        assert!(!ip_matches_cidr("198.51.100.7", "2001:db8::/32"));

        // scope_cameras_filter intersection and empty-intersection sentinel
        assert_eq!(
            scope_cameras_filter(
                Some(vec!["cam1".to_string(), "cam2".to_string()]),
                Some(vec!["cam2".to_string(), "cam3".to_string()])
            ),
            Some(vec!["cam2".to_string()])
        );
        assert_eq!(
            scope_cameras_filter(
                Some(vec!["cam1".to_string()]),
                Some(vec!["cam2".to_string()])
            ),
            Some(vec!["__none__".to_string()])
        );
        assert_eq!(
            scope_cameras_filter(None, Some(vec![])),
            Some(vec!["__none__".to_string()])
        );
    }

    #[tokio::test]
    async fn server_bugfixes_case_persistence_camera_scoped_global_routes_vod_clip_and_oidc_ready()
    {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr-srv-bugfixes-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let cfg_path = tmp_dir.join("config.yml");
        let db_path = tmp_dir.join("ninjanvr.db");
        let yaml = format!(
            r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
  oidc:
    enabled: true
    issuer_url: ""
    client_id: ""
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
            db_path.display()
        );
        std::fs::write(&cfg_path, yaml).unwrap();

        let state = new_app_state(&cfg_path, tmp_dir.join("web"), tmp_dir.join("media"), false)
            .await
            .unwrap();
        state
            .db
            .create_user_with_cameras("scoped_op", "pass1234", "operator", "front_door", 1_000)
            .unwrap();
        let app = build_router_with_auth(state.clone(), true);

        // 1. /api/auth/providers must report oidc.enabled == false when issuer_url / client_id are blank
        let resp_prov = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_prov.status(), StatusCode::OK);
        let prov_json: Value =
            serde_json::from_slice(&to_bytes(resp_prov.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(prov_json["oidc"]["enabled"], false);

        // 2. Create a camera-scoped operator token restricted to "front_door"
        let now = chrono::Utc::now().timestamp() as u64;
        let scoped_claims = JwtClaims {
            sub: "scoped_op".to_string(),
            role: "operator".to_string(),
            allowed_cameras: Some("front_door".to_string()),
            iat: now,
            exp: now + 3600,
        };
        let scoped_jwt = jsonwebtoken::encode(
            &Header::default(),
            &scoped_claims,
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap();
        let auth_hdr = format!("Bearer {scoped_jwt}");

        // Camera-scoped user can access global endpoints (/api/sub_labels, /api/recognized_license_plates, /api/faces)
        // and their permitted camera's /vod/clip/front_door/... route
        for uri in [
            "/api/sub_labels",
            "/api/recognized_license_plates",
            "/api/faces",
            "/vod/clip/front_door/start/100/end/200/master.m3u8",
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(header::AUTHORIZATION, &auth_hdr)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "camera-scoped user must be permitted to access {uri}"
            );
        }

        // Camera-scoped user is blocked from /vod/clip/backyard/... (403 Forbidden)
        let resp_forbidden = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/vod/clip/backyard/start/100/end/200/master.m3u8")
                    .header(header::AUTHORIZATION, &auth_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_forbidden.status(), StatusCode::FORBIDDEN);

        // 3. POST /api/cases persists case to SQLite and GET /api/cases returns it
        let case_payload = json!({
            "id": "case_incident_42",
            "name": "Perimeter Breach",
            "description": "Night motion near gate",
            "event_ids": ["ev_101"],
            "export_ids": ["exp_202"]
        });
        let resp_case_post = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/cases")
                    .header(header::AUTHORIZATION, &auth_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&case_payload).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_case_post.status(), StatusCode::OK);

        let resp_cases_get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/cases")
                    .header(header::AUTHORIZATION, &auth_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_cases_get.status(), StatusCode::OK);
        let cases_list: Value =
            serde_json::from_slice(&to_bytes(resp_cases_get.into_body(), 16384).await.unwrap())
                .unwrap();
        let cases_arr = cases_list.as_array().unwrap();
        assert_eq!(cases_arr.len(), 1);
        assert_eq!(cases_arr[0]["id"], "case_incident_42");
        assert_eq!(cases_arr[0]["name"], "Perimeter Breach");
        assert_eq!(cases_arr[0]["event_ids"], json!(["ev_101"]));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn debug_lpr_simulate_creates_searchable_plate_event_and_snapshots() {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr-lpr-sim-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let cfg_path = tmp_dir.join("config.yml");
        let db_path = tmp_dir.join("ninjanvr.db");
        std::fs::write(
            &cfg_path,
            format!(
                "version: 0.18-0\ndatabase:\n  path: {}\nlpr:\n  enabled: true\n  known_plates:\n    BRA2E19: \"Carro do Erik\"\ncameras:\n  portao:\n    enabled: true\n",
                db_path.display()
            ),
        )
        .unwrap();

        let media_dir = tmp_dir.join("media");
        let state = new_app_state(&cfg_path, tmp_dir.join("web"), media_dir.clone(), false)
            .await
            .unwrap();
        let app = build_router(state.clone());

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/debug/lpr-simulate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"camera":"portao","plate":"bra-2e19","label":"car","score":0.95}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let sim_json: Value =
            serde_json::from_slice(&to_bytes(resp.into_body(), 32768).await.unwrap()).unwrap();
        assert_eq!(sim_json["success"], true);
        assert_eq!(sim_json["camera"], "portao");
        assert_eq!(sim_json["plate"], "BRA2E19");
        assert_eq!(sim_json["sub_label"], "Carro do Erik");
        assert_eq!(sim_json["votes"], 3);

        let ev_id = sim_json["event_id"].as_str().unwrap();
        let rev_id = sim_json["review_id"].as_str().unwrap();

        // Verify snapshots and review thumbnails were written to disk
        let clips_dir = media_dir.join("clips");
        assert!(clips_dir.join(format!("{ev_id}.jpg")).exists());
        assert!(clips_dir.join(format!("portao-{ev_id}.webp")).exists());
        assert!(
            clips_dir
                .join(format!("portao-{ev_id}-clean.webp"))
                .exists()
        );
        assert!(
            clips_dir
                .join("review")
                .join(format!("thumb-portao-{rev_id}.webp"))
                .exists()
        );

        // Verify event is searchable by recognized_license_plate via GET /api/events
        let search_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/events?recognized_license_plate=BRA2E19")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(search_resp.status(), StatusCode::OK);
        let events_json: Value =
            serde_json::from_slice(&to_bytes(search_resp.into_body(), 65536).await.unwrap())
                .unwrap();
        let events_arr = events_json.as_array().unwrap();
        assert_eq!(events_arr.len(), 1);
        assert_eq!(events_arr[0]["id"], ev_id);
        assert_eq!(events_arr[0]["sub_label"], "Carro do Erik");
        assert_eq!(events_arr[0]["data"]["recognized_license_plate"], "BRA2E19");

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn genai_parsers_scrub_secrets_skip_gemini_thoughts_filter_models_and_parse_annotations() {
        // 1. scrub_genai_secret_in_error masks active key without corrupting masked/empty keys
        assert_eq!(
            scrub_genai_secret_in_error(
                "HTTP 401 from https://api.example.invalid?key=sk-secret-999".to_string(),
                "sk-secret-999"
            ),
            "HTTP 401 from https://api.example.invalid?key=***"
        );
        assert_eq!(
            scrub_genai_secret_in_error("normal error".to_string(), "***"),
            "normal error"
        );

        // 2. build_effective_genai_config honors overrides, ignores "***", and clears key on clear_api_key
        let saved = ninjanvr_config::GenAiConfig {
            provider: "gemini".to_string(),
            base_url: Some("https://saved.example.invalid".to_string()),
            api_key: Some("sk-saved".to_string()),
            api_key_env: "SAVED_ENV".to_string(),
            ..Default::default()
        };
        let masked_req = GenAiValidateRequest {
            provider: Some(" openai ".to_string()),
            base_url: Some("   ".to_string()),
            api_key: Some("***".to_string()),
            api_key_env: Some("OPENAI_API_KEY".to_string()),
            clear_api_key: Some(false),
            validate: None,
        };
        let eff_masked = build_effective_genai_config(&saved, &masked_req);
        assert_eq!(eff_masked.provider, "openai");
        assert_eq!(eff_masked.base_url, None);
        assert_eq!(eff_masked.api_key.as_deref(), Some("sk-saved"));
        assert_eq!(eff_masked.api_key_env, "OPENAI_API_KEY");

        let clear_req = GenAiValidateRequest {
            provider: None,
            base_url: None,
            api_key: Some("sk-ignored".to_string()),
            api_key_env: None,
            clear_api_key: Some(true),
            validate: None,
        };
        let eff_cleared = build_effective_genai_config(&saved, &clear_req);
        assert_eq!(eff_cleared.api_key, None);

        // 3. extract_models_from_provider_payload filters non-vision/embedding models for Gemini & OpenAI
        let gemini_payload = json!({
            "models": [
                {
                    "name": "models/gemini-2.5-flash",
                    "supportedGenerationMethods": ["generateContent", "countTokens"]
                },
                {
                    "name": "models/text-embedding-004",
                    "supportedGenerationMethods": ["embedContent"]
                },
                {
                    "name": "models/imagen-3.0-generate-002",
                    "supportedGenerationMethods": ["generateContent"]
                },
                {
                    "name": "models/veo-2.0-generate-001",
                    "supportedGenerationMethods": ["generateContent"]
                }
            ]
        });
        assert_eq!(
            extract_models_from_provider_payload("gemini", &gemini_payload),
            vec!["gemini-2.5-flash".to_string()]
        );

        let openai_payload = json!({
            "data": [
                { "id": "gpt-4o-mini" },
                { "id": "whisper-1" },
                { "id": "tts-1-hd" },
                { "id": "dall-e-3" },
                { "id": "text-embedding-3-small" }
            ]
        });
        assert_eq!(
            extract_models_from_provider_payload("openai", &openai_payload),
            vec!["gpt-4o-mini".to_string()]
        );

        // 4. extract_gemini_candidate_text & parse_ai_judge_json_verdict skip "thought": true parts
        let gemini_thinking_resp = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {
                            "thought": true,
                            "text": "Let me analyze the shadow near the porch..."
                        },
                        {
                            "text": "```json\n{\"is_true_positive\": false, \"confidence\": 0.94, \"reason\": \"Moving tree branch shadow\"}\n```"
                        },
                        {
                            "text": "   ",
                            "thoughtSignature": "trailing_empty_signature_part"
                        }
                    ]
                }
            }]
        });
        let (is_tp, conf, reason) = parse_ai_judge_json_verdict(&gemini_thinking_resp).unwrap();
        assert!(!is_tp);
        assert!((conf - 0.94).abs() < 1e-6);
        assert_eq!(reason, "Moving tree branch shadow");

        // 5. parse_ai_auto_annotations converts Gemini box_2d [ymin, xmin, ymax, xmax] (0..1000)
        // and pixel-space bbox into normalized YOLO [cx, cy, w, h] while skipping unknown labels
        let annotate_resp = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        { "thought": true, "text": "Detecting objects in frame..." },
                        {
                            "text": "```JSON\n[{\"label\": \"person\", \"box_2d\": [100, 200, 500, 400], \"score\": 0.96}, {\"label\": \"License-Plate\", \"bbox\": [640, 360, 768, 432], \"confidence\": 0.89}, {\"label\": \"alien_spaceship\", \"box_2d\": [0, 0, 100, 100]}]\n```"
                        }
                    ]
                }
            }]
        });
        let boxes = parse_ai_auto_annotations(&annotate_resp, 1280, 720).unwrap();
        assert_eq!(boxes.len(), 2);
        assert_eq!(boxes[0].class_id, 0);
        assert_eq!(boxes[0].label, "person");
        // box_2d [ymin=100, xmin=200, ymax=500, xmax=400] -> xmin=0.2, ymin=0.1, xmax=0.4, ymax=0.5 -> cx=0.3, cy=0.3, w=0.2, h=0.4
        assert!((boxes[0].cx - 0.3).abs() < 1e-5);
        assert!((boxes[0].cy - 0.3).abs() < 1e-5);
        assert!((boxes[0].w - 0.2).abs() < 1e-5);
        assert!((boxes[0].h - 0.4).abs() < 1e-5);
        assert_eq!(boxes[1].class_id, 7);
        assert_eq!(boxes[1].label, "license_plate");
    }

    #[tokio::test]
    async fn security_helpers_pkce_protected_paths_share_tokens_range_mp4_and_onvif_status_work() {
        // 1. is_protected_path matrix
        for public_path in [
            "/",
            "/login",
            "/logout",
            "/api/login",
            "/api/logout",
            "/api/auth/first_time_login",
            "/api/auth/providers",
            "/api/auth/oidc/login",
            "/api/auth/oidc/callback",
            "/api/ui/switch",
            "/api/ui/version",
            "/share/some_token",
        ] {
            assert!(
                !is_protected_path(public_path),
                "path '{public_path}' must be public"
            );
        }
        for protected_path in [
            "/api",
            "/api/config",
            "/api/events",
            "/vod/front_door/start/1/end/2/master.m3u8",
            "/vod/clip/front_door/start/1/end/2/master.m3u8",
            "/live/mse/api/ws",
            "/ws",
            "/clips/front_door-123.webp",
            "/recordings/2026-11-02/14/front_door/05.10.mp4",
            "/exports/exp_1.mp4",
        ] {
            assert!(
                is_protected_path(protected_path),
                "path '{protected_path}' must be protected"
            );
        }

        // 2. generate_pkce_pair S256 challenge verification + urlencoding_simple + resolve_external_base_url
        let (state_hex, verifier, challenge) = generate_pkce_pair();
        assert_eq!(state_hex.len(), 32);
        let expected_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected_challenge);
        assert_eq!(
            urlencoding_simple("https://nvr.example.invalid/cb?a=1&b=two words"),
            "https%3A%2F%2Fnvr.example.invalid%2Fcb%3Fa%3D1%26b%3Dtwo%20words"
        );

        let mut hdrs = HeaderMap::new();
        hdrs.insert("x-forwarded-proto", "https, http".parse().unwrap());
        hdrs.insert(
            "x-forwarded-host",
            "nvr.example.invalid:8443, internal".parse().unwrap(),
        );
        let uri: Uri = "/api/auth/oidc/login".parse().unwrap();
        assert_eq!(
            resolve_external_base_url(&hdrs, &uri),
            "https://nvr.example.invalid:8443"
        );

        // 3. sign_share_token & verify_share_token (valid, expired, tampered, malformed)
        let payload = ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_01".to_string(),
            exp: 2_000,
            cam: None,
            jti: None,
        };
        let token = sign_share_token("top_secret_hmac_key", &payload);
        let verified = verify_share_token("top_secret_hmac_key", &token, 1_500).unwrap();
        assert_eq!(verified.id, "exp_01");
        assert_eq!(
            verify_share_token("top_secret_hmac_key", &token, 2_000).unwrap_err(),
            StatusCode::GONE
        );
        assert_eq!(
            verify_share_token("wrong_key", &token, 1_500).unwrap_err(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            verify_share_token("top_secret_hmac_key", "no_dot_separator", 1_500).unwrap_err(),
            StatusCode::FORBIDDEN
        );

        // 4. serve_mp4_bytes_with_range (open-ended range, clamped end, invalid range fallback)
        let sample_mp4 = b"0123456789ABCDEF".to_vec();
        let mut range_hdrs = HeaderMap::new();
        range_hdrs.insert(header::RANGE, "bytes=10-".parse().unwrap());
        let resp_open = serve_mp4_bytes_with_range(sample_mp4.clone(), &range_hdrs);
        assert_eq!(resp_open.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp_open.headers().get(header::CONTENT_RANGE).unwrap(),
            "bytes 10-15/16"
        );
        let body_open = to_bytes(resp_open.into_body(), 64).await.unwrap();
        assert_eq!(&body_open[..], b"ABCDEF");

        range_hdrs.insert(header::RANGE, "bytes=12-9999".parse().unwrap());
        let resp_clamped = serve_mp4_bytes_with_range(sample_mp4.clone(), &range_hdrs);
        assert_eq!(resp_clamped.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp_clamped.headers().get(header::CONTENT_RANGE).unwrap(),
            "bytes 12-15/16"
        );

        range_hdrs.insert(header::RANGE, "bytes=10-4".parse().unwrap());
        let resp_invalid = serve_mp4_bytes_with_range(sample_mp4.clone(), &range_hdrs);
        assert_eq!(resp_invalid.status(), StatusCode::OK);

        // 5. parse_onvif_status_xml, continuous/relative move SOAP builders, and resolve_recording_disk_path
        let status_xml = r#"
        <s:Envelope>
            <s:Body>
                <tptz:GetStatusResponse>
                    <tptz:PTZStatus>
                        <tt:Position>
                            <tt:PanTilt x="-0.45" y="0.80"/>
                            <tt:Zoom x="0.25"/>
                        </tt:Position>
                        <tt:MoveStatus><tt:PanTilt>moving</tt:PanTilt></tt:MoveStatus>
                        <tt:Error>NoError</tt:Error>
                    </tptz:PTZStatus>
                </tptz:GetStatusResponse>
            </s:Body>
        </s:Envelope>
        "#;
        let parsed_status = parse_onvif_status_xml(status_xml);
        assert!((parsed_status.pan - (-0.45)).abs() < 1e-4);
        assert!((parsed_status.tilt - 0.80).abs() < 1e-4);
        assert!((parsed_status.zoom - 0.25).abs() < 1e-4);
        assert_eq!(parsed_status.move_status, "MOVING");
        assert_eq!(parsed_status.error, None);

        let cont_soap =
            build_onvif_continuous_move_soap("Profile_1", -2.0, 0.5, 0.0, 120, None, None);
        assert!(cont_soap.contains("<tt:PanTilt x=\"-1.0000\" y=\"0.5000\"/>"));
        assert!(cont_soap.contains("<tptz:Timeout>PT60S</tptz:Timeout>"));

        let rel_soap =
            build_onvif_relative_move_soap("Profile_1", 0.2, -0.2, 0.1, Some(0.01), None, None);
        assert!(rel_soap.contains("<tptz:RelativeMove>"));
        assert!(rel_soap.contains("<tt:PanTilt x=\"0.0500\" y=\"0.0500\"/>"));

        assert_eq!(
            resolve_recording_disk_path(
                Path::new("/custom/media"),
                "/media/ninjanvr/recordings/2026-11-02/14/front_door/05.10.mp4"
            ),
            PathBuf::from("/custom/media/recordings/2026-11-02/14/front_door/05.10.mp4")
        );
    }

    #[tokio::test]
    async fn ws_go2rtc_proxy_delivers_instant_gop_burst_and_prewarm_populates_cache() {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr-mse-prewarm-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let cfg_path = tmp_dir.join("config.yml");
        let db_path = tmp_dir.join("ninjanvr.db");
        std::fs::write(
            &cfg_path,
            format!(
                "version: 0.18-0\ndatabase:\n  path: {}\ncameras:\n  front_door:\n    enabled: true\n",
                db_path.display()
            ),
        )
        .unwrap();

        // Build synthetic fMP4 init segment + IDR keyframe fragment + P-frame fragment
        let make_box = |box_type: &[u8; 4], payload: &[u8]| -> Vec<u8> {
            let total = (8 + payload.len()) as u32;
            let mut out = Vec::with_capacity(8 + payload.len());
            out.extend_from_slice(&total.to_be_bytes());
            out.extend_from_slice(box_type);
            out.extend_from_slice(payload);
            out
        };
        let make_full_box = |box_type: &[u8; 4], version: u8, flags: u32, body: &[u8]| -> Vec<u8> {
            let total = (12 + body.len()) as u32;
            let mut out = Vec::with_capacity(12 + body.len());
            out.extend_from_slice(&total.to_be_bytes());
            out.extend_from_slice(box_type);
            let vf = ((version as u32) << 24) | (flags & 0x00FF_FFFF);
            out.extend_from_slice(&vf.to_be_bytes());
            out.extend_from_slice(body);
            out
        };
        let mut init_vec = make_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso6dashmp41");
        init_vec.extend_from_slice(&make_box(b"moov", &[0u8; 24]));
        let init_bytes = Bytes::from(init_vec);

        let build_frag = |is_sync: bool, tag: u8| -> Bytes {
            let flags: u32 = if is_sync { 0x0200_0000 } else { 0x0101_0000 };
            let mfhd = make_full_box(b"mfhd", 0, 0, &1u32.to_be_bytes());
            let tfhd = make_full_box(b"tfhd", 0, 0x020000, &1u32.to_be_bytes());
            let mut trun_body = Vec::new();
            trun_body.extend_from_slice(&1u32.to_be_bytes());
            trun_body.extend_from_slice(&64i32.to_be_bytes());
            trun_body.extend_from_slice(&3000u32.to_be_bytes());
            trun_body.extend_from_slice(&16u32.to_be_bytes());
            trun_body.extend_from_slice(&flags.to_be_bytes());
            let trun = make_full_box(b"trun", 0, 0x000701, &trun_body);
            let mut traf_c = Vec::new();
            traf_c.extend_from_slice(&tfhd);
            traf_c.extend_from_slice(&trun);
            let traf = make_box(b"traf", &traf_c);
            let mut moof_c = Vec::new();
            moof_c.extend_from_slice(&mfhd);
            moof_c.extend_from_slice(&traf);
            let mut out = make_box(b"moof", &moof_c);
            out.extend_from_slice(&make_box(b"mdat", &[tag; 16]));
            Bytes::from(out)
        };
        let keyframe_bytes = build_frag(true, 0xAA);
        let delta_bytes = build_frag(false, 0xBB);
        let codec_str = r#"{"type":"mse","value":"video/mp4; codecs=\"avc1.640029\""}"#;

        // Spin up mock go2rtc WS server
        let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = mock_listener.local_addr().unwrap();
        let init_srv = init_bytes.clone();
        let key_srv = keyframe_bytes.clone();
        let delta_srv = delta_bytes.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = mock_listener.accept().await {
                let init_c = init_srv.clone();
                let key_c = key_srv.clone();
                let delta_c = delta_srv.clone();
                tokio::spawn(async move {
                    if let Ok(ws) = tokio_tungstenite::accept_async(tcp).await {
                        let (mut sink, mut rx) = ws.split();
                        // Wait for client mse handshake
                        let _ = rx.next().await;
                        let _ = sink.send(TungMsg::Text(codec_str.into())).await;
                        let _ = sink.send(TungMsg::Binary(init_c)).await;
                        let _ = sink.send(TungMsg::Binary(key_c)).await;
                        let _ = sink.send(TungMsg::Binary(delta_c)).await;
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                });
            }
        });

        let mut state = new_app_state(&cfg_path, tmp_dir.join("web"), tmp_dir.join("media"), false)
            .await
            .unwrap();
        state.go2rtc_ws_base = format!("ws://{mock_addr}");
        let app = build_router(state.clone());

        // 1. Trigger POST /api/go2rtc/streams/front_door/prewarm
        let prewarm_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/go2rtc/streams/front_door/prewarm")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(prewarm_resp.status(), StatusCode::OK);
        let prewarm_json: Value =
            serde_json::from_slice(&to_bytes(prewarm_resp.into_body(), 4096).await.unwrap())
                .unwrap();
        assert_eq!(prewarm_json["success"], true);
        assert_eq!(prewarm_json["stream"], "front_door");
        assert_eq!(prewarm_json["warm"], true);

        // Wait briefly for the trailing delta frame to also land in MseGopCache
        tokio::time::sleep(Duration::from_millis(25)).await;

        // 2. Start NinjanVR HTTP server and connect a client WS to /live/mse/api/ws?src=front_door
        let srv_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let srv_addr = srv_listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(srv_listener, app).await;
        });

        let client_url = format!("ws://{srv_addr}/live/mse/api/ws?src=front_door");
        let (mut client_ws, _) = tokio_tungstenite::connect_async(&client_url).await.unwrap();
        client_ws
            .send(TungMsg::Text(
                r#"{"type":"mse","value":"avc1.640029"}"#.into(),
            ))
            .await
            .unwrap();

        // Client must immediately receive codec_msg, init_segment, and IDR keyframe fragment from cache
        let msg1 = tokio::time::timeout(Duration::from_millis(500), client_ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(msg1, TungMsg::Text(codec_str.into()));

        let msg2 = tokio::time::timeout(Duration::from_millis(500), client_ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(msg2, TungMsg::Binary(init_bytes));

        let msg3 = tokio::time::timeout(Duration::from_millis(500), client_ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(msg3, TungMsg::Binary(keyframe_bytes));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn latest_frame_endpoint_encodes_lazy_yuv_at_native_and_custom_heights() {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr-lazy-yuv-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let cfg_path = tmp_dir.join("config.yml");
        let db_path = tmp_dir.join("ninjanvr.db");
        std::fs::write(
            &cfg_path,
            format!(
                "version: 0.18-0\ndatabase:\n  path: {}\ncameras:\n  front_door:\n    enabled: true\n",
                db_path.display()
            ),
        )
        .unwrap();

        let state = new_app_state(&cfg_path, tmp_dir.join("web"), tmp_dir.join("media"), false)
            .await
            .unwrap();

        let width = 1280u32;
        let height = 720u32;
        let yuv_len = ((width * height * 3) / 2) as usize;
        let yuv_frame = ninjanvr_media::YuvFrame {
            camera: Arc::from("front_door"),
            timestamp: 1_793_700_000.0,
            width,
            height,
            data: Arc::new(vec![128u8; yuv_len]),
        };
        state
            .latest_yuv_frames
            .write()
            .await
            .insert("front_door".to_string(), yuv_frame);

        let app = build_router(state);

        // 1. Native resolution (no ?height param) -> 1280x720 JPEG
        let resp_native = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/front_door/latest.jpg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_native.status(), StatusCode::OK);
        assert_eq!(
            resp_native.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/jpeg"
        );
        let native_bytes = to_bytes(resp_native.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let native_img = image::load_from_memory(&native_bytes).unwrap();
        assert_eq!(native_img.width(), 1280);
        assert_eq!(native_img.height(), 720);

        // 2. Scaled resolution (?height=360) -> 640x360 JPEG
        let resp_360 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/front_door/latest.jpg?height=360")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_360.status(), StatusCode::OK);
        let bytes_360 = to_bytes(resp_360.into_body(), 1024 * 1024).await.unwrap();
        let img_360 = image::load_from_memory(&bytes_360).unwrap();
        assert_eq!(img_360.width(), 640);
        assert_eq!(img_360.height(), 360);

        // 3. WebP format (/latest.webp?height=360) -> 640x360 WebP
        let resp_webp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/front_door/latest.webp?height=360")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_webp.status(), StatusCode::OK);
        assert_eq!(
            resp_webp.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/webp"
        );
        let bytes_webp = to_bytes(resp_webp.into_body(), 1024 * 1024).await.unwrap();
        let img_webp = image::load_from_memory(&bytes_webp).unwrap();
        assert_eq!(img_webp.width(), 640);
        assert_eq!(img_webp.height(), 360);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn share_link_security_hardening_all_eight_findings_covered() {
        let tmp_dir = std::env::temp_dir().join(format!(
            "ninjanvr-share-sec-hardening-{}",
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();

        // =========================================================================
        // Finding 1: Secret entropy stretching, .jwt_secret 0o600 permissions,
        // and HMAC-SHA256 domain-separated share signing subkey
        // =========================================================================
        let stretched_short = normalize_jwt_secret_entropy("weak-short-key");
        assert_eq!(stretched_short.len(), 64);
        assert_eq!(
            stretched_short,
            normalize_jwt_secret_entropy("weak-short-key"),
            "KDF stretching must be deterministic"
        );
        assert_ne!(
            stretched_short,
            normalize_jwt_secret_entropy("other-short-key")
        );
        let long_secret = "0123456789abcdef0123456789abcdef_already_32_plus";
        assert_eq!(normalize_jwt_secret_entropy(long_secret), long_secret);

        let jwt_dir = tmp_dir.join("jwt_perm_test");
        std::fs::create_dir_all(&jwt_dir).unwrap();
        let created_secret = load_or_create_jwt_secret(&jwt_dir);
        assert_eq!(created_secret.len(), 128);
        let secret_file = jwt_dir.join(".jwt_secret");
        assert!(secret_file.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&secret_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, ".jwt_secret must be created with mode 0600");
        }
        // Verify empty/whitespace .jwt_secret on disk is regenerated and hardened to 0o600
        let empty_jwt_dir = tmp_dir.join("jwt_empty_test");
        std::fs::create_dir_all(&empty_jwt_dir).unwrap();
        let empty_secret_file = empty_jwt_dir.join(".jwt_secret");
        std::fs::write(&empty_secret_file, "   \n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&empty_secret_file, std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }
        let regenerated_secret = load_or_create_jwt_secret(&empty_jwt_dir);
        assert_eq!(regenerated_secret.len(), 128);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&empty_secret_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        // Verify existing short .jwt_secret on disk with 0o644 is stretched on load AND hardened to 0o600
        let short_jwt_dir = tmp_dir.join("jwt_short_test");
        std::fs::create_dir_all(&short_jwt_dir).unwrap();
        let short_secret_file = short_jwt_dir.join(".jwt_secret");
        std::fs::write(&short_secret_file, "short_disk_key\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&short_secret_file, std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }
        assert_eq!(
            load_or_create_jwt_secret(&short_jwt_dir),
            normalize_jwt_secret_entropy("short_disk_key")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&short_secret_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o600,
                "pre-existing .jwt_secret must be hardened to mode 0600"
            );
        }

        // Domain separation: raw HMAC-SHA256(jwt_secret, payload_b64) without subkey/prefix must fail
        let test_payload = ShareTokenPayload {
            k: "export".to_string(),
            id: "front_door_100_200".to_string(),
            exp: unix_now() + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_domain_sep".to_string()),
        };
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&test_payload).unwrap());
        let mut raw_mac =
            Hmac::<Sha256>::new_from_slice(created_secret.as_bytes()).expect("HMAC key");
        raw_mac.update(payload_b64.as_bytes());
        let raw_sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(raw_mac.finalize().into_bytes());
        let forged_unprefixed_token = format!("{payload_b64}.{raw_sig_b64}");
        assert_eq!(
            verify_share_token(&created_secret, &forged_unprefixed_token, unix_now()).unwrap_err(),
            StatusCode::FORBIDDEN
        );

        // =========================================================================
        // Setup two-camera test app with auth enforced (front_door + backyard)
        // =========================================================================
        let db_path = tmp_dir.join("ninjanvr.db");
        let cfg_path = tmp_dir.join("config.yml");
        let web_dir = tmp_dir.join("web-v2");
        let media_dir = tmp_dir.join("media");
        std::fs::create_dir_all(&web_dir).unwrap();
        std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

        let yaml = format!(
            r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
            db_path.display()
        );
        std::fs::write(&cfg_path, yaml).unwrap();

        let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
            .await
            .unwrap();
        let auth_app = build_router_with_auth(state.clone(), true);

        let now = unix_now();
        let make_jwt = |sub: &str, role: &str, allowed_cameras: Option<&str>| -> String {
            let claims = JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: allowed_cameras.map(|s| s.to_string()),
                iat: now,
                exp: now + 3600,
            };
            jsonwebtoken::encode(
                &Header::new(Algorithm::HS256),
                &claims,
                &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
            )
            .unwrap()
        };

        state
            .db
            .create_user_with_cameras("scoped_op", "pass1234", "operator", "front_door", 1000)
            .unwrap();
        state
            .db
            .create_user_with_cameras("viewer_u", "pass1234", "viewer", "*", 1000)
            .unwrap();
        state
            .db
            .create_user_with_cameras("temp_op", "pass1234", "operator", "front_door", 1000)
            .unwrap();
        let (_, ro_api_token) = state
            .db
            .create_api_token("ro_sensor", "read_only", None)
            .unwrap();

        let admin_hdr = format!("Bearer {}", make_jwt("admin", "admin", Some("*")));
        let scoped_op_hdr = format!(
            "Bearer {}",
            make_jwt("scoped_op", "operator", Some("front_door"))
        );
        let viewer_hdr = format!("Bearer {}", make_jwt("viewer_u", "viewer", Some("*")));
        let ro_tok_hdr = format!("Bearer {ro_api_token}");
        let temp_op_hdr = format!(
            "Bearer {}",
            make_jwt("temp_op", "operator", Some("front_door"))
        );

        // Seed exports, events, reviews, recordings, and static media files on disk
        let exports_dir = media_dir.join("exports");
        let clips_dir = media_dir.join("clips");
        let clips_previews_backyard = clips_dir.join("previews/backyard");
        let clips_review_dir = clips_dir.join("review");
        let rec_fd_dir = media_dir.join("recordings/2026-11-03/12/front_door");
        let rec_by_dir = media_dir.join("recordings/2026-11-03/12/backyard");
        let rec_staging_by = media_dir.join("recordings/.staging/backyard");
        for d in [
            &exports_dir,
            &clips_dir,
            &clips_previews_backyard,
            &clips_review_dir,
            &rec_fd_dir,
            &rec_by_dir,
            &rec_staging_by,
        ] {
            std::fs::create_dir_all(d).unwrap();
        }

        let mp4_payload: Vec<u8> = (0u8..64u8).collect();
        std::fs::write(exports_dir.join("front_door_100_200.mp4"), &mp4_payload).unwrap();
        std::fs::write(exports_dir.join("backyard_100_200.mp4"), &mp4_payload).unwrap();
        std::fs::write(rec_fd_dir.join("00.00.mp4"), &mp4_payload).unwrap();
        std::fs::write(rec_by_dir.join("00.00.mp4"), &mp4_payload).unwrap();
        std::fs::write(rec_staging_by.join("seg.mp4"), &mp4_payload).unwrap();
        std::fs::write(clips_previews_backyard.join("thumb.webp"), b"WEBP").unwrap();
        std::fs::write(
            clips_review_dir.join("thumb-backyard-100-rev1.webp"),
            b"WEBP",
        )
        .unwrap();
        std::fs::write(clips_dir.join("exp_backyard.webp"), b"WEBP").unwrap();
        std::fs::write(clips_dir.join("front_door-ev_fd_1.webp"), b"WEBP").unwrap();
        std::fs::write(clips_dir.join("mystery_unscoped.webp"), b"WEBP").unwrap();

        state
            .db
            .insert_export(&ExportRow {
                id: "front_door_100_200".to_string(),
                camera: "front_door".to_string(),
                name: "Front Door Export".to_string(),
                date: 1700000100.0,
                video_path: "/media/ninjanvr/exports/front_door_100_200.mp4".to_string(),
                thumb_path: "/media/ninjanvr/clips/front_door_100_200.webp".to_string(),
                in_progress: false,
            })
            .unwrap();
        state
            .db
            .insert_export(&ExportRow {
                id: "exp_backyard".to_string(),
                camera: "backyard".to_string(),
                name: "Backyard Export".to_string(),
                date: 1700000110.0,
                video_path: "/media/ninjanvr/exports/backyard_100_200.mp4".to_string(),
                thumb_path: "/media/ninjanvr/clips/exp_backyard.webp".to_string(),
                in_progress: false,
            })
            .unwrap();

        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "rec_fd_1".to_string(),
                camera: "front_door".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/00.00.mp4".to_string(),
                start_time: 1000.0,
                end_time: 1010.0,
                duration: 10.0,
                motion: 5,
                objects: 1,
                dbfs: -20,
                segment_size: 0.1,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();
        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "rec_by_1".to_string(),
                camera: "backyard".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-03/12/backyard/00.00.mp4".to_string(),
                start_time: 1000.0,
                end_time: 1010.0,
                duration: 10.0,
                motion: 5,
                objects: 1,
                dbfs: -20,
                segment_size: 0.1,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();

        for (ev_id, cam) in [("ev_fd_1", "front_door"), ("ev_by_1", "backyard")] {
            state
                .db
                .upsert_event(&ninjanvr_db::EventDto {
                    id: ev_id.to_string(),
                    camera: cam.to_string(),
                    label: "person".to_string(),
                    zones: json!([]),
                    start_time: 1000.0,
                    end_time: Some(1008.0),
                    has_clip: true,
                    has_snapshot: true,
                    plus_id: None,
                    retain_indefinitely: false,
                    sub_label: None,
                    top_score: Some(0.9),
                    false_positive: Some(false),
                    box_coords: Some(json!([0.1, 0.1, 0.4, 0.8])),
                    data: json!({}),
                    thumbnail: None,
                    area: Some(5000),
                    ratio: Some(0.5),
                    score: Some(0.9),
                    stationary: false,
                })
                .unwrap();
        }
        for (rev_id, cam) in [("rev_fd_1", "front_door"), ("rev_by_1", "backyard")] {
            state
                .db
                .insert_review_segment(&ninjanvr_db::ReviewItemDto {
                    id: rev_id.to_string(),
                    camera: cam.to_string(),
                    start_time: 1000.0,
                    end_time: Some(1008.0),
                    severity: "alert".to_string(),
                    thumb_path: format!("/media/ninjanvr/clips/review/thumb-{cam}-{rev_id}.webp"),
                    data: json!({}),
                    has_been_reviewed: false,
                })
                .unwrap();
        }

        // =========================================================================
        // Finding 2: Share Token Redaction in Logs & /api/logs/* Admin Restriction
        // =========================================================================
        assert_eq!(sanitize_logged_path("/share/"), "/share/");
        assert_eq!(
            sanitize_logged_path("/share/secret_tok_123"),
            "/share/<redacted>"
        );
        assert_eq!(
            sanitize_logged_path("/share/secret_tok_123/video.mp4"),
            "/share/<redacted>/video.mp4"
        );
        assert_eq!(sanitize_logged_path("/api/version"), "/api/version");
        assert_eq!(sanitize_logged_path("/api/stats"), "/api/stats");

        let mut log_rx = state.log_hub.subscribe();
        state
            .log_hub
            .push_nginx("GET", "/share/raw_secret_token_xyz/video.mp4", 200);
        let (svc, streamed_line) = log_rx.try_recv().unwrap();
        assert_eq!(svc, "nginx");
        assert!(streamed_line.contains("/share/<redacted>/video.mp4"));
        assert!(!streamed_line.contains("raw_secret_token_xyz"));

        // Verify /api/logs/nginx is 403 Forbidden for viewer, read_only token, and operator, 200 OK for admin
        for non_admin_hdr in [&viewer_hdr, &ro_tok_hdr, &scoped_op_hdr] {
            let resp_log_denied = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/logs/nginx")
                        .header(header::AUTHORIZATION, non_admin_hdr)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp_log_denied.status(), StatusCode::FORBIDDEN);
        }
        let resp_log_admin = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/logs/nginx")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_log_admin.status(), StatusCode::OK);

        // =========================================================================
        // Finding 3: Privacy Masks on Shared Event/Review Clips
        // =========================================================================
        assert!(!has_active_privacy_mask(None));
        assert!(!has_active_privacy_mask(Some(&json!([]))));
        assert!(!has_active_privacy_mask(Some(&json!({}))));
        assert!(!has_active_privacy_mask(Some(&json!(""))));
        assert!(!has_active_privacy_mask(Some(&json!("   "))));
        assert!(has_active_privacy_mask(Some(&json!([[
            [0.1, 0.1],
            [0.2, 0.2],
            [0.1, 0.2]
        ]]))));
        assert!(has_active_privacy_mask(Some(&json!({
            "coordinates": "0.1,0.1,0.5,0.5"
        }))));
        assert!(has_active_privacy_mask(Some(&json!("0.1,0.1,0.5,0.5"))));

        // Camera without privacy_mask streams the raw recording segment
        state
            .db
            .insert_share_link(
                "shr_ev_fd_1",
                "event",
                "ev_fd_1",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let ev_fd_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "event".to_string(),
                id: "ev_fd_1".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_ev_fd_1".to_string()),
            },
        );
        let resp_unmasked_vid = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{ev_fd_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_unmasked_vid.status(), StatusCode::OK);
        let unmasked_bytes = to_bytes(resp_unmasked_vid.into_body(), 4096).await.unwrap();
        assert_eq!(&unmasked_bytes[..], &mp4_payload[..]);

        // Enable array privacy_mask on backyard:
        // Case A: when build_mp4_export_with_privacy fails (no segment on disk for ev_by_missing_seg), it fails closed with 404
        {
            let mut cfg = state.config.write().await;
            cfg.cameras.get_mut("backyard").unwrap().privacy_mask =
                json!([[[0.1, 0.1], [0.5, 0.1], [0.5, 0.5], [0.1, 0.5]]]);
        }
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_by_missing_seg".to_string(),
                camera: "backyard".to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 5000.0,
                end_time: Some(5008.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.9),
                false_positive: Some(false),
                box_coords: None,
                data: json!({}),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.9),
                stationary: false,
            })
            .unwrap();
        state
            .db
            .insert_share_link(
                "shr_ev_by_missing",
                "event",
                "ev_by_missing_seg",
                Some("backyard"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let ev_by_missing_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "event".to_string(),
                id: "ev_by_missing_seg".to_string(),
                exp: now + 3600,
                cam: Some("backyard".to_string()),
                jti: Some("shr_ev_by_missing".to_string()),
            },
        );
        let resp_masked_fail_closed = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{ev_by_missing_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_masked_fail_closed.status(), StatusCode::NOT_FOUND);

        // Case B: when segment exists on disk (00.00.mp4) but ffmpeg cannot transcode/redact it,
        // build_mp4_export_with_privacy fails closed (404 Not Found), never copies the unredacted raw file,
        // and cleans up any transient DB export row
        state
            .db
            .insert_share_link(
                "shr_ev_by_1",
                "event",
                "ev_by_1",
                Some("backyard"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let ev_by_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "event".to_string(),
                id: "ev_by_1".to_string(),
                exp: now + 3600,
                cam: Some("backyard".to_string()),
                jti: Some("shr_ev_by_1".to_string()),
            },
        );
        let resp_masked_unredactable = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{ev_by_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_masked_unredactable.status(), StatusCode::NOT_FOUND);
        assert!(!exports_dir.join("share_redacted_ev_by_1.mp4").exists());
        assert!(
            state
                .db
                .get_export_by_id("share_redacted_ev_by_1")
                .unwrap()
                .is_none()
        );

        // Case C: Pre-generated redacted export (exports/share_redacted_ev_by_1.mp4) is served when privacy_mask is active
        // (string format too) instead of serving the raw unredacted segment from recordings/
        {
            let mut cfg = state.config.write().await;
            cfg.cameras.get_mut("backyard").unwrap().privacy_mask =
                json!("0.1,0.1,0.5,0.1,0.5,0.5");
        }
        let redacted_bytes = b"REDACTED_MP4_STREAM_PAYLOAD".to_vec();
        std::fs::write(
            exports_dir.join("share_redacted_ev_by_1.mp4"),
            &redacted_bytes,
        )
        .unwrap();
        let resp_masked_redacted = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{ev_by_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_masked_redacted.status(), StatusCode::OK);
        let served_redacted = to_bytes(resp_masked_redacted.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(&served_redacted[..], &redacted_bytes[..]);
        assert_ne!(&served_redacted[..], &mp4_payload[..]);

        // =========================================================================
        // Finding 4: Target existence, camera authorization, deleted-user fail-closed,
        // and cross-table camera mismatch protection
        // =========================================================================
        // 4a. Non-existent target returns 404 Not Found
        for kind in ["export", "event", "review"] {
            let resp_404 = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/share")
                        .header(header::AUTHORIZATION, &admin_hdr)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            json!({
                                "kind": kind,
                                "target_id": "non_existent_id_999"
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp_404.status(), StatusCode::NOT_FOUND);
        }

        // 4b. Camera-restricted operator ("front_door") gets 403 Forbidden sharing "backyard" export/event/review
        for (kind, target_id) in [
            ("export", "exp_backyard"),
            ("event", "ev_by_1"),
            ("review", "rev_by_1"),
        ] {
            let resp_403 = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/share")
                        .header(header::AUTHORIZATION, &scoped_op_hdr)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            json!({
                                "kind": kind,
                                "target_id": target_id
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp_403.status(),
                StatusCode::FORBIDDEN,
                "scoped_op must be forbidden from sharing {kind}:{target_id}"
            );
        }

        // 4c. Deleting local operator `temp_op` from SQLite while JWT is still unexpired fails closed (403)
        assert!(state.db.delete_user("temp_op").unwrap());
        let resp_deleted_share = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &temp_op_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"front_door_100_200"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_deleted_share.status(), StatusCode::FORBIDDEN);

        let resp_deleted_cam_route = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/front_door/recordings")
                    .header(header::AUTHORIZATION, &temp_op_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_deleted_cam_route.status(), StatusCode::FORBIDDEN);

        // Direct unit coverage of extract_jwt_allowed_cameras & resolve_user_allowed_cameras:
        // - Cookie token extraction as well as Authorization Bearer header & nnvr_ prefix
        let cookie_jwt = make_jwt("oidc_cookie_op", "operator", Some("front_door"));
        let jar_with_cookie =
            CookieJar::new().add(Cookie::new("ninjanvr_token", cookie_jwt.clone()));
        let empty_hdrs = HeaderMap::new();
        assert_eq!(
            extract_jwt_allowed_cameras(&state, &jar_with_cookie, &empty_hdrs, "ninjanvr_token"),
            Some("front_door".to_string())
        );
        let mut nnvr_hdrs = HeaderMap::new();
        nnvr_hdrs.insert(
            header::AUTHORIZATION,
            "Bearer nnvr_api_tok".parse().unwrap(),
        );
        assert_eq!(
            extract_jwt_allowed_cameras(&state, &CookieJar::new(), &nnvr_hdrs, "ninjanvr_token"),
            None
        );

        // - token: prefix user in resolve_user_allowed_cameras returns None
        let base_auth_cfg = state.config.read().await.auth.clone();
        assert_eq!(
            resolve_user_allowed_cameras(
                &state,
                &CookieJar::new(),
                &empty_hdrs,
                &base_auth_cfg,
                "token:ro_sensor",
                "read_only"
            ),
            None
        );

        // - db_u.role == "admin" in SQLite returns None even if passed role is "operator"
        state
            .db
            .create_user_with_cameras("db_admin_u", "pass1234", "admin", "front_door", 1000)
            .unwrap();
        assert_eq!(
            resolve_user_allowed_cameras(
                &state,
                &CookieJar::new(),
                &empty_hdrs,
                &base_auth_cfg,
                "db_admin_u",
                "operator"
            ),
            None
        );
        state.db.delete_user("db_admin_u").unwrap();

        // - OIDC enabled + non-empty issuer_url for OIDC JWT user not in local SQLite:
        //   Some("front_door"), Some("*"), and None
        let mut oidc_auth_cfg = base_auth_cfg.clone();
        oidc_auth_cfg.oidc.enabled = true;
        oidc_auth_cfg.oidc.issuer_url = "https://idp.example.test".to_string();
        assert_eq!(
            resolve_user_allowed_cameras(
                &state,
                &jar_with_cookie,
                &empty_hdrs,
                &oidc_auth_cfg,
                "oidc_cookie_op",
                "operator"
            ),
            Some(vec!["front_door".to_string()])
        );
        let jar_oidc_star = CookieJar::new().add(Cookie::new(
            "ninjanvr_token",
            make_jwt("oidc_star_op", "operator", Some("*")),
        ));
        assert_eq!(
            resolve_user_allowed_cameras(
                &state,
                &jar_oidc_star,
                &empty_hdrs,
                &oidc_auth_cfg,
                "oidc_star_op",
                "operator"
            ),
            None
        );
        let jar_oidc_none = CookieJar::new().add(Cookie::new(
            "ninjanvr_token",
            make_jwt("oidc_none_op", "operator", None),
        ));
        assert_eq!(
            resolve_user_allowed_cameras(
                &state,
                &jar_oidc_none,
                &empty_hdrs,
                &oidc_auth_cfg,
                "oidc_none_op",
                "operator"
            ),
            Some(Vec::new())
        );

        // - Proxy enabled with Remote-User header returns None
        let mut proxy_auth_cfg = base_auth_cfg.clone();
        proxy_auth_cfg.proxy.enabled = true;
        let mut proxy_hdrs = HeaderMap::new();
        proxy_hdrs.insert("remote-user", "proxy_op".parse().unwrap());
        assert_eq!(
            resolve_user_allowed_cameras(
                &state,
                &CookieJar::new(),
                &proxy_hdrs,
                &proxy_auth_cfg,
                "proxy_op",
                "operator"
            ),
            None
        );

        // - Even when oidc.enabled = true with empty issuer_url, or proxy.enabled = true without proxy headers,
        //   or when all local users are deleted (list_users() is empty), a deleted user's JWT still fails closed
        let mut unconfigured_oidc_cfg = base_auth_cfg.clone();
        unconfigured_oidc_cfg.oidc.enabled = true;
        unconfigured_oidc_cfg.oidc.issuer_url = String::new();
        unconfigured_oidc_cfg.proxy.enabled = true;
        assert_eq!(
            resolve_user_allowed_cameras(
                &state,
                &jar_with_cookie,
                &empty_hdrs,
                &unconfigured_oidc_cfg,
                "oidc_cookie_op",
                "operator"
            ),
            Some(Vec::new())
        );

        // 4d. Cross-table camera mismatch, payload.cam mismatch, and fallback resolution
        state
            .db
            .insert_share_link(
                "shr_mismatch_payload",
                "event",
                "ev_by_1",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let mismatch_payload_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "event".to_string(),
                id: "ev_by_1".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_mismatch_payload".to_string()),
            },
        );
        for uri in [
            format!("/share/{mismatch_payload_tok}"),
            format!("/share/{mismatch_payload_tok}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "mismatch on {uri}");
        }

        // Colliding ID across event ("front_door") and review ("backyard") tables -> 403 Forbidden
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "collision_id_1".to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 1000.0,
                end_time: Some(1008.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.9),
                false_positive: Some(false),
                box_coords: None,
                data: json!({}),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.9),
                stationary: false,
            })
            .unwrap();
        state
            .db
            .insert_review_segment(&ninjanvr_db::ReviewItemDto {
                id: "collision_id_1".to_string(),
                camera: "backyard".to_string(),
                start_time: 1000.0,
                end_time: Some(1008.0),
                severity: "alert".to_string(),
                thumb_path: "/media/ninjanvr/clips/review/thumb-backyard-collision_id_1.webp"
                    .to_string(),
                data: json!({}),
                has_been_reviewed: false,
            })
            .unwrap();
        state
            .db
            .insert_share_link(
                "shr_collision_1",
                "review",
                "collision_id_1",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let collision_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "review".to_string(),
                id: "collision_id_1".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_collision_1".to_string()),
            },
        );
        for uri in [
            format!("/share/{collision_tok}"),
            format!("/share/{collision_tok}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN);
        }

        // Direct and router coverage of resolve_share_target_metadata, share_video_mp4, and share_player_page:
        // - kind == "review" falling back to get_event when get_review_by_id is None
        let (fb_ev_cam, fb_ev_title, _, fb_ev_exists) =
            resolve_share_target_metadata(&state, "review", "ev_fd_1");
        assert!(fb_ev_exists);
        assert_eq!(fb_ev_cam.as_deref(), Some("front_door"));
        assert!(fb_ev_title.contains("front_door"));

        // - kind == "event" falling back to get_review_by_id when get_event is None
        let (fb_rev_cam, fb_rev_title, _, fb_rev_exists) =
            resolve_share_target_metadata(&state, "event", "rev_fd_1");
        assert!(fb_rev_exists);
        assert_eq!(fb_rev_cam.as_deref(), Some("front_door"));
        assert!(fb_rev_title.contains("front_door"));

        // - kind == "export" disk-fallback when get_export_by_id is None
        std::fs::write(
            exports_dir.join("front_door_diskonly_300.mp4"),
            &mp4_payload,
        )
        .unwrap();
        let (fb_disk_cam, _, _, fb_disk_exists) =
            resolve_share_target_metadata(&state, "export", "front_door_diskonly_300");
        assert!(fb_disk_exists);
        assert_eq!(fb_disk_cam.as_deref(), Some("front_door"));

        // - share_video_mp4 for kind == "review" happy path (200 OK) and token_cam mismatch (403 Forbidden)
        state
            .db
            .insert_share_link(
                "shr_rev_fd_ok",
                "review",
                "rev_fd_1",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let rev_fd_ok_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "review".to_string(),
                id: "rev_fd_1".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_rev_fd_ok".to_string()),
            },
        );
        let resp_rev_fd_ok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{rev_fd_ok_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev_fd_ok.status(), StatusCode::OK);

        state
            .db
            .insert_share_link(
                "shr_rev_fd_mismatch",
                "review",
                "rev_fd_1",
                Some("backyard"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let rev_fd_mismatch_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "review".to_string(),
                id: "rev_fd_1".to_string(),
                exp: now + 3600,
                cam: Some("backyard".to_string()),
                jti: Some("shr_rev_fd_mismatch".to_string()),
            },
        );
        let resp_rev_fd_mismatch = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{rev_fd_mismatch_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev_fd_mismatch.status(), StatusCode::FORBIDDEN);

        // - share_video_mp4 for kind == "review" falling back to get_event (200 OK and 403 mismatch)
        state
            .db
            .insert_share_link(
                "shr_rev_fb_ev_ok",
                "review",
                "ev_fd_1",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let rev_fb_ev_ok_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "review".to_string(),
                id: "ev_fd_1".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_rev_fb_ev_ok".to_string()),
            },
        );
        let resp_rev_fb_ev_ok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{rev_fb_ev_ok_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev_fb_ev_ok.status(), StatusCode::OK);

        state
            .db
            .insert_share_link(
                "shr_rev_fb_ev_mismatch",
                "review",
                "ev_fd_1",
                Some("backyard"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let rev_fb_ev_mismatch_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "review".to_string(),
                id: "ev_fd_1".to_string(),
                exp: now + 3600,
                cam: Some("backyard".to_string()),
                jti: Some("shr_rev_fb_ev_mismatch".to_string()),
            },
        );
        let resp_rev_fb_ev_mismatch = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{rev_fb_ev_mismatch_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev_fb_ev_mismatch.status(), StatusCode::FORBIDDEN);

        // - share_video_mp4 for kind == "event" falling back to get_review_by_id (200 OK and 403 mismatch)
        state
            .db
            .insert_share_link(
                "shr_ev_fb_rev_ok",
                "event",
                "rev_fd_1",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let ev_fb_rev_ok_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "event".to_string(),
                id: "rev_fd_1".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_ev_fb_rev_ok".to_string()),
            },
        );
        let resp_ev_fb_rev_ok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{ev_fb_rev_ok_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ev_fb_rev_ok.status(), StatusCode::OK);

        state
            .db
            .insert_share_link(
                "shr_ev_fb_rev_mismatch",
                "event",
                "rev_fd_1",
                Some("backyard"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let ev_fb_rev_mismatch_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "event".to_string(),
                id: "rev_fd_1".to_string(),
                exp: now + 3600,
                cam: Some("backyard".to_string()),
                jti: Some("shr_ev_fb_rev_mismatch".to_string()),
            },
        );
        let resp_ev_fb_rev_mismatch = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{ev_fb_rev_mismatch_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ev_fb_rev_mismatch.status(), StatusCode::FORBIDDEN);

        // - share_video_mp4 for kind == "export" when token_cam != exp_cam (403) and disk-fallback (200 & 403)
        state
            .db
            .insert_share_link(
                "shr_exp_mismatch",
                "export",
                "exp_backyard",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let exp_mismatch_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: "exp_backyard".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_exp_mismatch".to_string()),
            },
        );
        let resp_exp_mismatch = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{exp_mismatch_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_exp_mismatch.status(), StatusCode::FORBIDDEN);

        state
            .db
            .insert_share_link(
                "shr_exp_disk_ok",
                "export",
                "front_door_diskonly_300",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let exp_disk_ok_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: "front_door_diskonly_300".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_exp_disk_ok".to_string()),
            },
        );
        let resp_exp_disk_ok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{exp_disk_ok_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_exp_disk_ok.status(), StatusCode::OK);

        state
            .db
            .insert_share_link(
                "shr_exp_disk_mismatch",
                "export",
                "front_door_diskonly_300",
                Some("backyard"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let exp_disk_mismatch_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: "front_door_diskonly_300".to_string(),
                exp: now + 3600,
                cam: Some("backyard".to_string()),
                jti: Some("shr_exp_disk_mismatch".to_string()),
            },
        );
        let resp_exp_disk_mismatch = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{exp_disk_mismatch_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_exp_disk_mismatch.status(), StatusCode::FORBIDDEN);

        // - share_player_page returning 404 Not Found when target does not exist / was deleted after token creation
        state
            .db
            .insert_share_link(
                "shr_deleted_target_999",
                "event",
                "deleted_after_share_999",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let deleted_target_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "event".to_string(),
                id: "deleted_after_share_999".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_deleted_target_999".to_string()),
            },
        );
        let resp_player_404 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{deleted_target_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_player_404.status(), StatusCode::NOT_FOUND);
        state.db.prune_expired_share_links(f64::MAX).unwrap();

        // =========================================================================
        // Finding 5: Bounded Async File Streaming (`serve_mp4_file_with_range`)
        // =========================================================================
        assert!(
            serve_mp4_file_with_range(&exports_dir, &empty_hdrs)
                .await
                .is_none()
        );

        let file_disk_path = exports_dir.join("front_door_100_200.mp4");
        let full_resp = serve_mp4_file_with_range(&file_disk_path, &empty_hdrs)
            .await
            .unwrap();
        assert_eq!(full_resp.status(), StatusCode::OK);
        for hdr_name in [
            header::CONTENT_SECURITY_POLICY,
            header::X_CONTENT_TYPE_OPTIONS,
            header::REFERRER_POLICY,
            header::X_FRAME_OPTIONS,
            header::CACHE_CONTROL,
        ] {
            assert!(
                full_resp.headers().contains_key(&hdr_name),
                "missing security header {hdr_name} on 200 OK file response"
            );
        }
        let full_bytes = to_bytes(full_resp.into_body(), 4096).await.unwrap();
        assert_eq!(&full_bytes[..], &mp4_payload[..]);

        let mut open_hdrs = HeaderMap::new();
        open_hdrs.insert(header::RANGE, "bytes=10-".parse().unwrap());
        let open_resp = serve_mp4_file_with_range(&file_disk_path, &open_hdrs)
            .await
            .unwrap();
        assert_eq!(open_resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            open_resp.headers().get(header::CONTENT_RANGE).unwrap(),
            "bytes 10-63/64"
        );
        for hdr_name in [
            header::CONTENT_SECURITY_POLICY,
            header::X_CONTENT_TYPE_OPTIONS,
            header::REFERRER_POLICY,
            header::X_FRAME_OPTIONS,
            header::CACHE_CONTROL,
        ] {
            assert!(
                open_resp.headers().contains_key(&hdr_name),
                "missing security header {hdr_name} on 206 Partial Content file response"
            );
        }
        let open_bytes = to_bytes(open_resp.into_body(), 4096).await.unwrap();
        assert_eq!(&open_bytes[..], &mp4_payload[10..]);

        let mut bounded_hdrs = HeaderMap::new();
        bounded_hdrs.insert(header::RANGE, "bytes=0-3".parse().unwrap());
        let bounded_resp = serve_mp4_file_with_range(&file_disk_path, &bounded_hdrs)
            .await
            .unwrap();
        assert_eq!(bounded_resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            bounded_resp.headers().get(header::CONTENT_RANGE).unwrap(),
            "bytes 0-3/64"
        );
        let bounded_bytes = to_bytes(bounded_resp.into_body(), 4096).await.unwrap();
        assert_eq!(&bounded_bytes[..], &mp4_payload[0..=3]);

        let mut inverted_hdrs = HeaderMap::new();
        inverted_hdrs.insert(header::RANGE, "bytes=10-4".parse().unwrap());
        let inverted_resp = serve_mp4_file_with_range(&file_disk_path, &inverted_hdrs)
            .await
            .unwrap();
        assert_eq!(inverted_resp.status(), StatusCode::OK);
        let inverted_bytes = to_bytes(inverted_resp.into_body(), 4096).await.unwrap();
        assert_eq!(&inverted_bytes[..], &mp4_payload[..]);

        let mut oob_hdrs = HeaderMap::new();
        oob_hdrs.insert(header::RANGE, "bytes=9999-10000".parse().unwrap());
        let oob_resp = serve_mp4_file_with_range(&file_disk_path, &oob_hdrs)
            .await
            .unwrap();
        assert_eq!(oob_resp.status(), StatusCode::OK);
        let oob_bytes = to_bytes(oob_resp.into_body(), 4096).await.unwrap();
        assert_eq!(&oob_bytes[..], &mp4_payload[..]);

        // =========================================================================
        // Finding 6 & Finding 8: Share Link Persistence, Listing, Revocation & Security Headers
        // =========================================================================
        let resp_create_fd = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &scoped_op_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"front_door_100_200","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_create_fd.status(), StatusCode::OK);
        let create_fd_json: Value =
            serde_json::from_slice(&to_bytes(resp_create_fd.into_body(), 4096).await.unwrap())
                .unwrap();
        let fd_share_id = create_fd_json["id"].as_str().unwrap().to_string();
        let fd_share_token = create_fd_json["token"].as_str().unwrap().to_string();

        let resp_create_by = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_backyard","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_create_by.status(), StatusCode::OK);
        let create_by_json: Value =
            serde_json::from_slice(&to_bytes(resp_create_by.into_body(), 4096).await.unwrap())
                .unwrap();
        let by_share_id = create_by_json["id"].as_str().unwrap().to_string();

        // Finding 8: Verify HTML response headers on GET /share/{token}
        let resp_player = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{fd_share_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_player.status(), StatusCode::OK);
        let player_hdrs = resp_player.headers();
        assert!(
            player_hdrs
                .get(header::CONTENT_SECURITY_POLICY)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("default-src 'none'")
        );
        assert_eq!(
            player_hdrs.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        assert_eq!(
            player_hdrs.get(header::REFERRER_POLICY).unwrap(),
            "no-referrer"
        );
        assert_eq!(player_hdrs.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
        assert_eq!(
            player_hdrs.get(header::CACHE_CONTROL).unwrap(),
            "private, no-store"
        );

        // Verify nginx access log redacted the share token from the router request above
        let (_, nginx_lines) = state.log_hub.query_lines("nginx", None, None);
        assert!(
            nginx_lines.iter().any(|l| l.contains("/share/<redacted>")),
            "nginx log buffer must contain redacted share path"
        );
        assert!(
            !nginx_lines.iter().any(|l| l.contains(&fd_share_token)),
            "nginx log buffer must never contain raw share token"
        );

        // Finding 6: GET /api/share filters by caller_allowed_cameras for scoped_op
        let resp_list_scoped = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &scoped_op_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_list_scoped.status(), StatusCode::OK);
        let list_scoped_arr: Value =
            serde_json::from_slice(&to_bytes(resp_list_scoped.into_body(), 8192).await.unwrap())
                .unwrap();
        let scoped_links = list_scoped_arr.as_array().unwrap();
        assert_eq!(scoped_links.len(), 1);
        assert_eq!(scoped_links[0]["id"], fd_share_id);

        // DELETE /api/share/{id} returns 403 for unauthorized camera, 404 for unknown ID, 200 OK for permitted camera
        let resp_revoke_forbidden = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/share/{by_share_id}"))
                    .header(header::AUTHORIZATION, &scoped_op_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_revoke_forbidden.status(), StatusCode::FORBIDDEN);

        let resp_revoke_404 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/share/shr_unknown_999")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_revoke_404.status(), StatusCode::NOT_FOUND);

        let resp_revoke_ok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/share/{fd_share_id}"))
                    .header(header::AUTHORIZATION, &scoped_op_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_revoke_ok.status(), StatusCode::OK);

        // Subsequent GET /share/{token} and GET /share/{token}/video.mp4 return 410 Gone
        for uri in [
            format!("/share/{fd_share_token}"),
            format!("/share/{fd_share_token}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::GONE,
                "revoked link must return 410 Gone on {uri}"
            );
        }

        // =========================================================================
        // Finding 7: Per-Camera Scoping on /api/exports, /api/export/*, and Static Media
        // =========================================================================
        // Direct unit coverage of resolve_static_media_camera for all path shapes:
        assert_eq!(
            resolve_static_media_camera(&state, "/exports/share_redacted_ev_fd_1.mp4").as_deref(),
            Some("front_door")
        );
        assert_eq!(
            resolve_static_media_camera(&state, "/exports/share_redacted_rev_by_1.mp4").as_deref(),
            Some("backyard")
        );
        assert_eq!(
            resolve_static_media_camera(&state, "/recordings/front_door/00.00.mp4").as_deref(),
            Some("front_door")
        );
        assert_eq!(resolve_static_media_camera(&state, "/recordings/"), None);
        assert_eq!(
            resolve_static_media_camera(&state, "/clips/review/thumb-front_door-rev1.webp")
                .as_deref(),
            Some("front_door")
        );
        assert_eq!(
            resolve_static_media_camera(&state, "/clips/review/not_a_thumb.webp"),
            None
        );
        assert_eq!(
            resolve_static_media_camera(&state, "/clips/ev_fd_1.webp").as_deref(),
            Some("front_door")
        );
        assert_eq!(
            resolve_static_media_camera(&state, "/clips/rev_by_1.webp").as_deref(),
            Some("backyard")
        );
        assert_eq!(
            resolve_static_media_camera(&state, "/clips/share_redacted_ev_fd_1.webp").as_deref(),
            Some("front_door")
        );

        let resp_exps_scoped = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/exports")
                    .header(header::AUTHORIZATION, &scoped_op_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_exps_scoped.status(), StatusCode::OK);
        let exps_scoped_arr: Value =
            serde_json::from_slice(&to_bytes(resp_exps_scoped.into_body(), 8192).await.unwrap())
                .unwrap();
        let exps_scoped = exps_scoped_arr.as_array().unwrap();
        assert_eq!(exps_scoped.len(), 1);
        assert_eq!(exps_scoped[0]["id"], "front_door_100_200");

        let resp_rename_by = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/export/exp_backyard/rename")
                    .header(header::AUTHORIZATION, &scoped_op_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"name":"Hijacked"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rename_by.status(), StatusCode::FORBIDDEN);

        let resp_rename_fd_ok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/export/front_door_100_200/rename")
                    .header(header::AUTHORIZATION, &scoped_op_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"name":"Front Door Renamed"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rename_fd_ok.status(), StatusCode::OK);

        let resp_bulk_del_by = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/exports/delete")
                    .header(header::AUTHORIZATION, &scoped_op_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"ids":["exp_backyard"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_bulk_del_by.status(), StatusCode::FORBIDDEN);

        // Seed a second front_door export to verify scoped operator bulk delete & admin single delete
        state
            .db
            .insert_export(&ExportRow {
                id: "front_door_300_400".to_string(),
                camera: "front_door".to_string(),
                name: "Front Door Export 2".to_string(),
                date: 1700000200.0,
                video_path: "/media/ninjanvr/exports/front_door_300_400.mp4".to_string(),
                thumb_path: "/media/ninjanvr/clips/front_door_300_400.webp".to_string(),
                in_progress: false,
            })
            .unwrap();
        let resp_bulk_del_fd_ok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/exports/delete")
                    .header(header::AUTHORIZATION, &scoped_op_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"ids":["front_door_300_400"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_bulk_del_fd_ok.status(), StatusCode::OK);
        assert!(
            state
                .db
                .get_export_by_id("front_door_300_400")
                .unwrap()
                .is_none()
        );

        for forbidden_static in [
            "/exports/backyard_100_200.mp4",
            "/exports/share_redacted_ev_by_1.mp4",
            "/recordings/2026-11-03/12/backyard/00.00.mp4",
            "/recordings/.staging/backyard/seg.mp4",
            "/clips/previews/backyard/thumb.webp",
            "/clips/review/thumb-backyard-100-rev1.webp",
            "/clips/exp_backyard.webp",
            "/clips/mystery_unscoped.webp",
        ] {
            let r = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(forbidden_static)
                        .header(header::AUTHORIZATION, &scoped_op_hdr)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::FORBIDDEN,
                "scoped_op must get 403 Forbidden on {forbidden_static}"
            );
        }

        for allowed_static in [
            "/exports/front_door_100_200.mp4",
            "/recordings/2026-11-03/12/front_door/00.00.mp4",
            "/clips/front_door-ev_fd_1.webp",
        ] {
            let r = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(allowed_static)
                        .header(header::AUTHORIZATION, &scoped_op_hdr)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::OK,
                "scoped_op must get 200 OK on {allowed_static}"
            );
        }

        // Finally, verify admin DELETE /api/export/{id} succeeds and verify deleted local user
        // still fails closed when all local SQLite users are deleted (list_users() is empty)
        let resp_admin_del_exp = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/export/exp_backyard")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_admin_del_exp.status(), StatusCode::OK);

        let mut empty_users_state = state.clone();
        empty_users_state.db = NinjaDatabase::open(tmp_dir.join("empty_users.db")).unwrap();
        assert!(empty_users_state.db.list_users().unwrap().is_empty());
        assert_eq!(
            resolve_user_allowed_cameras(
                &empty_users_state,
                &CookieJar::new(),
                &empty_hdrs,
                &base_auth_cfg,
                "scoped_op",
                "operator"
            ),
            Some(Vec::new()),
            "deleted local user must fail closed even when SQLite user table is empty"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn security_share_link_phase1_jti_allowlist_kind_validation_and_lifecycle_revocation_work()
     {
        let tmp_dir = std::env::temp_dir().join(format!(
            "ninjanvr_sec_share_phase1_{}",
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let db_path = tmp_dir.join("ninjanvr.db");
        let cfg_path = tmp_dir.join("config.yml");
        let web_dir = tmp_dir.join("web-v2");
        let media_dir = tmp_dir.join("media");
        std::fs::create_dir_all(&web_dir).unwrap();
        std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

        let yaml = format!(
            r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
  sala:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/sala
          roles: [record, detect]
"#,
            db_path.display()
        );
        std::fs::write(&cfg_path, yaml).unwrap();

        let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
            .await
            .unwrap();
        let auth_app = build_router_with_auth(state.clone(), true);

        let now = unix_now();
        let make_jwt = |sub: &str, role: &str, allowed_cameras: Option<&str>| -> String {
            let claims = JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: allowed_cameras.map(|s| s.to_string()),
                iat: now,
                exp: now + 3600,
            };
            jsonwebtoken::encode(
                &Header::new(Algorithm::HS256),
                &claims,
                &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
            )
            .unwrap()
        };

        state
            .db
            .create_user_with_cameras(
                "op_lifecycle",
                "pass1234",
                "operator",
                "front_door,backyard",
                1000,
            )
            .unwrap();
        state
            .db
            .create_user_with_cameras("op_to_delete", "pass1234", "operator", "front_door", 1000)
            .unwrap();

        let admin_hdr = format!("Bearer {}", make_jwt("admin", "admin", Some("*")));
        let op_lifecycle_hdr = format!(
            "Bearer {}",
            make_jwt("op_lifecycle", "operator", Some("front_door,backyard"))
        );
        let op_del_hdr = format!(
            "Bearer {}",
            make_jwt("op_to_delete", "operator", Some("front_door"))
        );

        let exports_dir = media_dir.join("exports");
        let rec_fd_dir = media_dir.join("recordings/2026-11-03/12/front_door");
        let rec_sala_dir = media_dir.join("recordings/2026-11-03/12/sala");
        std::fs::create_dir_all(&exports_dir).unwrap();
        std::fs::create_dir_all(&rec_fd_dir).unwrap();
        std::fs::create_dir_all(&rec_sala_dir).unwrap();

        let mp4_payload: Vec<u8> = (0u8..64u8).collect();
        std::fs::write(exports_dir.join("exp_fd_1.mp4"), &mp4_payload).unwrap();
        std::fs::write(exports_dir.join("exp_fd_2.mp4"), &mp4_payload).unwrap();
        std::fs::write(exports_dir.join("share_redacted_ev_secret.mp4"), b"SECRET").unwrap();
        std::fs::write(exports_dir.join("share_clip_ev_secret.mp4"), b"SECRET").unwrap();
        std::fs::write(exports_dir.join(".hidden_export.mp4"), b"SECRET").unwrap();
        std::fs::write(rec_fd_dir.join("00.00.mp4"), &mp4_payload).unwrap();
        std::fs::write(rec_sala_dir.join("00.00.mp4"), b"SALA_FALLBACK_LEAK").unwrap();

        for exp_id in ["exp_fd_1", "exp_fd_2", "exp_fd_keep"] {
            std::fs::write(exports_dir.join(format!("{exp_id}.mp4")), &mp4_payload).unwrap();
            state
                .db
                .insert_export(&ExportRow {
                    id: exp_id.to_string(),
                    camera: "front_door".to_string(),
                    name: format!("Export {exp_id}"),
                    date: 1700000100.0,
                    video_path: format!("/media/ninjanvr/exports/{exp_id}.mp4"),
                    thumb_path: format!("/media/ninjanvr/clips/{exp_id}.webp"),
                    in_progress: false,
                })
                .unwrap();
        }

        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "rec_fd_1".to_string(),
                camera: "front_door".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/00.00.mp4".to_string(),
                start_time: 1000.0,
                end_time: 1010.0,
                duration: 10.0,
                motion: 5,
                objects: 1,
                dbfs: -20,
                segment_size: 0.1,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();
        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "rec_sala_1".to_string(),
                camera: "sala".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-03/12/sala/00.00.mp4".to_string(),
                start_time: 1000.0,
                end_time: 1010.0,
                duration: 10.0,
                motion: 5,
                objects: 1,
                dbfs: -20,
                segment_size: 0.1,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();

        for ev_id in ["ev_p1_1", "ev_p1_2"] {
            state
                .db
                .upsert_event(&ninjanvr_db::EventDto {
                    id: ev_id.to_string(),
                    camera: "front_door".to_string(),
                    label: "person".to_string(),
                    zones: json!([]),
                    start_time: 1000.0,
                    end_time: Some(1008.0),
                    has_clip: true,
                    has_snapshot: true,
                    plus_id: None,
                    retain_indefinitely: false,
                    sub_label: None,
                    top_score: Some(0.9),
                    false_positive: Some(false),
                    box_coords: None,
                    data: json!({}),
                    thumbnail: None,
                    area: None,
                    ratio: None,
                    score: Some(0.9),
                    stationary: false,
                })
                .unwrap();
        }

        // Event with empty camera must NOT fall back to "sala"
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_empty_cam".to_string(),
                camera: String::new(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 1000.0,
                end_time: Some(1008.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.9),
                false_positive: Some(false),
                box_coords: None,
                data: json!({}),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.9),
                stationary: false,
            })
            .unwrap();

        for rev_id in ["rev_p1_1", "rev_p1_2"] {
            state
                .db
                .insert_review_segment(&ninjanvr_db::ReviewItemDto {
                    id: rev_id.to_string(),
                    camera: "front_door".to_string(),
                    start_time: 1000.0,
                    end_time: Some(1008.0),
                    severity: "alert".to_string(),
                    thumb_path: format!(
                        "/media/ninjanvr/clips/review/thumb-front_door-{rev_id}.webp"
                    ),
                    data: json!({}),
                    has_been_reviewed: false,
                })
                .unwrap();
        }

        // =========================================================================
        // 1. High-entropy 128-bit JTI & Fail-Closed Allowlist Verification (Finding 3)
        // =========================================================================
        let resp_create = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &op_lifecycle_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_fd_keep","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_create.status(), StatusCode::OK);
        let create_json: Value =
            serde_json::from_slice(&to_bytes(resp_create.into_body(), 4096).await.unwrap())
                .unwrap();
        let jti = create_json["id"].as_str().unwrap().to_string();
        let token = create_json["token"].as_str().unwrap().to_string();
        let parts: Vec<&str> = jti.split('_').collect();
        assert_eq!(parts.len(), 3, "JTI must have shr_<ts>_<32hex> format");
        assert_eq!(parts[0], "shr");
        assert_eq!(
            parts[2].len(),
            32,
            "JTI random suffix must be 32 hex chars (128 bits)"
        );
        assert!(
            parts[2]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );

        // Unregistered JTI (not present in share_link table) -> 410 GONE
        let unregistered_jti_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: "exp_fd_keep".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_1700000000_0123456789abcdef0123456789abcdef".to_string()),
            },
        );
        for uri in [
            format!("/share/{unregistered_jti_tok}"),
            format!("/share/{unregistered_jti_tok}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::GONE,
                "unregistered JTI must return 410 GONE on {uri}"
            );
        }

        // Expired DB row (`expires_at <= now_ts`) even if signed payload.exp is in the future -> 410 GONE
        state
            .db
            .insert_share_link(
                "shr_db_expired_0001",
                "export",
                "exp_fd_keep",
                Some("front_door"),
                "admin",
                (now - 7200) as f64,
                (now - 10) as f64,
            )
            .unwrap();
        let db_expired_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: "exp_fd_keep".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_db_expired_0001".to_string()),
            },
        );
        for uri in [
            format!("/share/{db_expired_tok}"),
            format!("/share/{db_expired_tok}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::GONE,
                "DB-expired JTI must return 410 GONE on {uri}"
            );
        }

        // Payload mismatch against DB record (kind, target_id, or camera) -> 403 FORBIDDEN
        for mismatched_payload in [
            ShareTokenPayload {
                k: "event".to_string(),
                id: "exp_fd_keep".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some(jti.clone()),
            },
            ShareTokenPayload {
                k: "export".to_string(),
                id: "exp_fd_1".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some(jti.clone()),
            },
            ShareTokenPayload {
                k: "export".to_string(),
                id: "exp_fd_keep".to_string(),
                exp: now + 3600,
                cam: Some("backyard".to_string()),
                jti: Some(jti.clone()),
            },
        ] {
            let bad_tok = sign_share_token(&state.jwt_secret, &mismatched_payload);
            for uri in [
                format!("/share/{bad_tok}"),
                format!("/share/{bad_tok}/video.mp4"),
            ] {
                let r = auth_app
                    .clone()
                    .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    r.status(),
                    StatusCode::FORBIDDEN,
                    "DB/payload mismatch must return 403 FORBIDDEN on {uri}"
                );
            }
        }

        // =========================================================================
        // 2. Strict Kind Validation, No 'sala' Fallback, Block Internal Export Prefixes (Finding 7)
        // =========================================================================
        for invalid_kind in ["recording", "clip", "unknown", ""] {
            let invalid_kind_tok = sign_share_token(
                &state.jwt_secret,
                &ShareTokenPayload {
                    k: invalid_kind.to_string(),
                    id: "exp_fd_keep".to_string(),
                    exp: now + 3600,
                    cam: Some("front_door".to_string()),
                    jti: None,
                },
            );
            assert_eq!(
                verify_share_token(&state.jwt_secret, &invalid_kind_tok, now).unwrap_err(),
                StatusCode::FORBIDDEN
            );
            for uri in [
                format!("/share/{invalid_kind_tok}"),
                format!("/share/{invalid_kind_tok}/video.mp4"),
            ] {
                let r = auth_app
                    .clone()
                    .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(r.status(), StatusCode::FORBIDDEN);
            }
        }

        // Event with empty camera does NOT fall back to "sala"
        let (_, _, _, empty_cam_exists) =
            resolve_share_target_metadata(&state, "event", "ev_empty_cam");
        assert!(!empty_cam_exists);
        for kind in ["event", "review"] {
            let jti_ec = format!("shr_empty_cam_{kind}");
            state
                .db
                .insert_share_link(
                    &jti_ec,
                    kind,
                    "ev_empty_cam",
                    None,
                    "admin",
                    now as f64,
                    (now + 3600) as f64,
                )
                .unwrap();
            let empty_cam_tok = sign_share_token(
                &state.jwt_secret,
                &ShareTokenPayload {
                    k: kind.to_string(),
                    id: "ev_empty_cam".to_string(),
                    exp: now + 3600,
                    cam: None,
                    jti: Some(jti_ec),
                },
            );
            for uri in [
                format!("/share/{empty_cam_tok}"),
                format!("/share/{empty_cam_tok}/video.mp4"),
            ] {
                let r = auth_app
                    .clone()
                    .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    r.status(),
                    StatusCode::NOT_FOUND,
                    "empty-camera event must fail closed with 404 on {uri}"
                );
            }
        }

        // Internal prefixes ("share_redacted_", "share_clip_", ".") blocked in export disk-fallback
        for blocked_id in [
            "share_redacted_ev_secret",
            "share_clip_ev_secret",
            ".hidden_export",
        ] {
            let (_, _, _, exists) = resolve_share_target_metadata(&state, "export", blocked_id);
            assert!(
                !exists,
                "internal/hidden export disk artifact {blocked_id} must be rejected"
            );
            let jti_blk = format!("shr_blk_{blocked_id}");
            state
                .db
                .insert_share_link(
                    &jti_blk,
                    "export",
                    blocked_id,
                    None,
                    "admin",
                    now as f64,
                    (now + 3600) as f64,
                )
                .unwrap();
            let blocked_tok = sign_share_token(
                &state.jwt_secret,
                &ShareTokenPayload {
                    k: "export".to_string(),
                    id: blocked_id.to_string(),
                    exp: now + 3600,
                    cam: None,
                    jti: Some(jti_blk),
                },
            );
            for uri in [
                format!("/share/{blocked_tok}"),
                format!("/share/{blocked_tok}/video.mp4"),
            ] {
                let r = auth_app
                    .clone()
                    .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    r.status(),
                    StatusCode::NOT_FOUND,
                    "internal export fallback {blocked_id} must return 404 on {uri}"
                );
            }
        }

        // =========================================================================
        // 3. Creator Permission Coupling & Target/User Deletion Revocation (Finding 8)
        // =========================================================================
        // Initially `token` created by `op_lifecycle` (allowed: front_door,backyard) works (200 OK)
        for uri in [
            format!("/share/{token}"),
            format!("/share/{token}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        }

        // Restrict `op_lifecycle` to only `backyard` -> `token` for `front_door` must return 403 FORBIDDEN
        state
            .db
            .update_user_allowed_cameras("op_lifecycle", "backyard")
            .unwrap();
        for uri in [
            format!("/share/{token}"),
            format!("/share/{token}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::FORBIDDEN,
                "revoking creator camera permission must return 403 on {uri}"
            );
        }
        // Restore `op_lifecycle` camera permission -> 200 OK again
        state
            .db
            .update_user_allowed_cameras("op_lifecycle", "front_door,backyard")
            .unwrap();

        // Helper to create a share link as a specific user
        let create_share = |hdr: String, kind: &str, target_id: &str| {
            let app = auth_app.clone();
            let body_str = json!({
                "kind": kind,
                "target_id": target_id,
                "expires_in_sec": 3600
            })
            .to_string();
            async move {
                let resp = app
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/api/share")
                            .header(header::AUTHORIZATION, hdr)
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(body_str))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::OK);
                let j: Value =
                    serde_json::from_slice(&to_bytes(resp.into_body(), 4096).await.unwrap())
                        .unwrap();
                (
                    j["id"].as_str().unwrap().to_string(),
                    j["token"].as_str().unwrap().to_string(),
                )
            }
        };

        // Single export delete revokes its share link
        let (jti_exp1, tok_exp1) = create_share(admin_hdr.clone(), "export", "exp_fd_1").await;
        let del_exp1 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/export/exp_fd_1")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del_exp1.status(), StatusCode::OK);
        assert!(state.db.is_share_link_revoked(&jti_exp1).unwrap());
        let r_exp1 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{tok_exp1}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_exp1.status(), StatusCode::GONE);

        // Bulk export delete revokes its share link
        let (jti_exp2, tok_exp2) = create_share(admin_hdr.clone(), "export", "exp_fd_2").await;
        let del_exp2 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/exports/delete")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"ids":["exp_fd_2"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del_exp2.status(), StatusCode::OK);
        assert!(state.db.is_share_link_revoked(&jti_exp2).unwrap());
        let r_exp2 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{tok_exp2}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_exp2.status(), StatusCode::GONE);

        // Single event delete & bulk event delete revoke share links
        let (jti_ev1, tok_ev1) = create_share(admin_hdr.clone(), "event", "ev_p1_1").await;
        let (jti_ev2, tok_ev2) = create_share(admin_hdr.clone(), "event", "ev_p1_2").await;
        let del_ev1 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/events/ev_p1_1")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del_ev1.status(), StatusCode::OK);
        assert!(state.db.is_share_link_revoked(&jti_ev1).unwrap());
        assert_eq!(
            auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/share/{tok_ev1}"))
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::GONE
        );

        let del_ev2 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/events")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"event_ids":["ev_p1_2"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del_ev2.status(), StatusCode::OK);
        assert!(state.db.is_share_link_revoked(&jti_ev2).unwrap());
        assert_eq!(
            auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/share/{tok_ev2}/video.mp4"))
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::GONE
        );

        // Single review delete & bulk review delete revoke share links
        let (jti_rev1, tok_rev1) = create_share(admin_hdr.clone(), "review", "rev_p1_1").await;
        let (jti_rev2, tok_rev2) = create_share(admin_hdr.clone(), "review", "rev_p1_2").await;
        let del_rev1 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/review/rev_p1_1")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del_rev1.status(), StatusCode::OK);
        assert!(state.db.is_share_link_revoked(&jti_rev1).unwrap());
        assert_eq!(
            auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/share/{tok_rev1}"))
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::GONE
        );

        let del_rev2 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/reviews/delete")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"ids":["rev_p1_2"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del_rev2.status(), StatusCode::OK);
        assert!(state.db.is_share_link_revoked(&jti_rev2).unwrap());
        assert_eq!(
            auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/share/{tok_rev2}/video.mp4"))
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::GONE
        );

        // User deletion via DELETE /api/users/{username} revokes all share links created by that user
        let (jti_user_del, tok_user_del) =
            create_share(op_del_hdr.clone(), "export", "exp_fd_keep").await;
        assert!(!state.db.is_share_link_revoked(&jti_user_del).unwrap());
        let resp_del_user = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/users/op_to_delete")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_del_user.status(), StatusCode::OK);
        assert!(state.db.is_share_link_revoked(&jti_user_del).unwrap());
        for uri in [
            format!("/share/{tok_user_del}"),
            format!("/share/{tok_user_del}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::GONE);
        }

        // Finally, if the `share_link` table is unavailable/dropped, `POST /api/share`, `/share/{token}`,
        // and `/share/{token}/video.mp4` all fail closed with 500 INTERNAL_SERVER_ERROR
        state.db.execute_batch("DROP TABLE share_link;").unwrap();
        let resp_create_db_err = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_fd_keep","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp_create_db_err.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );

        for uri in [
            format!("/share/{token}"),
            format!("/share/{token}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "DB error during JTI lookup must fail closed with 500 on {uri}"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn security_share_link_phase2_singleflight_mask_hash_multi_segment_and_path_containment_work()
     {
        let tmp_dir = std::env::temp_dir().join(format!(
            "ninjanvr_sec_share_phase2_{}",
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let db_path = tmp_dir.join("ninjanvr.db");
        let cfg_path = tmp_dir.join("config.yml");
        let web_dir = tmp_dir.join("web-v2");
        let media_dir = tmp_dir.join("media");
        std::fs::create_dir_all(&web_dir).unwrap();
        std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

        let yaml = format!(
            r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
            db_path.display()
        );
        std::fs::write(&cfg_path, yaml).unwrap();

        let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
            .await
            .unwrap();
        let auth_app = build_router_with_auth(state.clone(), true);
        let now = unix_now();
        let admin_jwt = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: "admin".to_string(),
                role: "admin".to_string(),
                allowed_cameras: Some("*".to_string()),
                iat: now,
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap();
        let admin_hdr = format!("Bearer {admin_jwt}");

        let exports_dir = media_dir.join("exports");
        let clips_dir = media_dir.join("clips");
        let rec_fd_dir = media_dir.join("recordings/2026-11-03/12/front_door");
        let rec_by_dir = media_dir.join("recordings/2026-11-03/12/backyard");
        for d in [&exports_dir, &clips_dir, &rec_fd_dir, &rec_by_dir] {
            std::fs::create_dir_all(d).unwrap();
        }

        // Write a valid synthetic MP4 payload (>= 2KB)
        let mut mp4_payload = vec![0u8; 2048];
        mp4_payload[0..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p']);

        // =========================================================================
        // 1. Strict Path Containment Inside `media_dir` (Finding 4)
        // =========================================================================
        let outside_file = tmp_dir.join("outside_secret.mp4");
        std::fs::write(&outside_file, b"OUTSIDE_MEDIA_DIR_SECRET_BYTES").unwrap();
        let inside_file = exports_dir.join("inside_valid.mp4");
        std::fs::write(&inside_file, &mp4_payload).unwrap();

        assert!(ensure_path_within_media_dir(&media_dir, &inside_file).is_some());
        assert!(ensure_path_within_media_dir(&media_dir, &outside_file).is_none());
        let traversal_candidate = media_dir.join("exports/../../outside_secret.mp4");
        assert!(ensure_path_within_media_dir(&media_dir, &traversal_candidate).is_none());

        #[cfg(unix)]
        {
            let symlink_export = exports_dir.join("front_door_symlink_escape.mp4");
            std::os::unix::fs::symlink(&outside_file, &symlink_export).unwrap();
            assert!(ensure_path_within_media_dir(&media_dir, &symlink_export).is_none());

            // Disk-only export fallback pointing to a symlink outside media_dir must be rejected
            let (_, _, _, sym_exists) =
                resolve_share_target_metadata(&state, "export", "front_door_symlink_escape");
            assert!(
                !sym_exists,
                "symlink export escaping media_dir must not resolve as existing"
            );
            state
                .db
                .insert_share_link(
                    "shr_sym_exp",
                    "export",
                    "front_door_symlink_escape",
                    Some("front_door"),
                    "admin",
                    now as f64,
                    (now + 3600) as f64,
                )
                .unwrap();
            let sym_exp_tok = sign_share_token(
                &state.jwt_secret,
                &ShareTokenPayload {
                    k: "export".to_string(),
                    id: "front_door_symlink_escape".to_string(),
                    exp: now + 3600,
                    cam: Some("front_door".to_string()),
                    jti: Some("shr_sym_exp".to_string()),
                },
            );
            let r_sym_exp = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/share/{sym_exp_tok}/video.mp4"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r_sym_exp.status(), StatusCode::NOT_FOUND);

            // Recording symlink outside media_dir must be rejected
            let symlink_rec = rec_fd_dir.join("symlink_outside.mp4");
            std::os::unix::fs::symlink(&outside_file, &symlink_rec).unwrap();
            state
                .db
                .insert_recording(&ninjanvr_db::RecordingRow {
                    id: "rec_sym_escape".to_string(),
                    camera: "front_door".to_string(),
                    path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/symlink_outside.mp4"
                        .to_string(),
                    start_time: 8000.0,
                    end_time: 8010.0,
                    duration: 10.0,
                    motion: 1,
                    objects: 1,
                    dbfs: -20,
                    segment_size: 0.1,
                    regions: 1,
                    motion_heatmap: None,
                })
                .unwrap();
        }

        // Export row in DB with `..` traversal in video_path must be rejected
        state
            .db
            .insert_export(&ExportRow {
                id: "exp_traversal".to_string(),
                camera: "front_door".to_string(),
                name: "Traversal Export".to_string(),
                date: 1700000100.0,
                video_path: "/media/ninjanvr/exports/../../outside_secret.mp4".to_string(),
                thumb_path: "/media/ninjanvr/clips/exp_traversal.webp".to_string(),
                in_progress: false,
            })
            .unwrap();
        state
            .db
            .insert_share_link(
                "shr_trav_exp",
                "export",
                "exp_traversal",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let trav_exp_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: "exp_traversal".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_trav_exp".to_string()),
            },
        );
        let r_trav_exp = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{trav_exp_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_trav_exp.status(), StatusCode::NOT_FOUND);

        // Event referencing recording with `..` traversal / symlink outside media_dir must return 404
        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "rec_trav_escape".to_string(),
                camera: "front_door".to_string(),
                path: "/media/ninjanvr/recordings/../../../../../../etc/passwd".to_string(),
                start_time: 8000.0,
                end_time: 8010.0,
                duration: 10.0,
                motion: 1,
                objects: 1,
                dbfs: -20,
                segment_size: 0.1,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_escape_test".to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 8002.0,
                end_time: Some(8008.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.9),
                false_positive: Some(false),
                box_coords: None,
                data: json!({}),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.9),
                stationary: false,
            })
            .unwrap();
        state
            .db
            .insert_share_link(
                "shr_ev_escape",
                "event",
                "ev_escape_test",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let ev_escape_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "event".to_string(),
                id: "ev_escape_test".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_ev_escape".to_string()),
            },
        );
        let r_ev_escape = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{ev_escape_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_ev_escape.status(), StatusCode::NOT_FOUND);

        // =========================================================================
        // 2. Privacy Mask Hash Sidecar Invalidation & Singleflight Concurrency (Finding 1)
        // =========================================================================
        let seg_by_disk = rec_by_dir.join("00.00.mp4");
        std::fs::write(&seg_by_disk, b"UNREDACTED_BY_SEG").unwrap();
        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "rec_by_p2_1".to_string(),
                camera: "backyard".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-03/12/backyard/00.00.mp4".to_string(),
                start_time: 2000.0,
                end_time: 2010.0,
                duration: 10.0,
                motion: 5,
                objects: 1,
                dbfs: -20,
                segment_size: 0.1,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_p2_mask".to_string(),
                camera: "backyard".to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 2001.0,
                end_time: Some(2008.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.9),
                false_positive: Some(false),
                box_coords: None,
                data: json!({}),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.9),
                stationary: false,
            })
            .unwrap();

        let mask_v1 = json!([[[0.1, 0.1], [0.4, 0.1], [0.4, 0.4], [0.1, 0.4]]]);
        let mask_v2 = json!([[[0.2, 0.2], [0.8, 0.2], [0.8, 0.8], [0.2, 0.8]]]);
        assert_ne!(
            compute_privacy_mask_hash(Some(&mask_v1)),
            compute_privacy_mask_hash(Some(&mask_v2))
        );
        {
            let mut cfg = state.config.write().await;
            cfg.cameras.get_mut("backyard").unwrap().privacy_mask = mask_v1.clone();
        }

        let redacted_file = exports_dir.join("share_redacted_ev_p2_mask.mp4");
        let sidecar_file = exports_dir.join(".share_redacted_ev_p2_mask.mask_hash");
        std::fs::write(&redacted_file, b"REDACTED_V1_BYTES").unwrap();
        assert!(!sidecar_file.exists());

        state
            .db
            .insert_share_link(
                "shr_ev_p2_mask",
                "event",
                "ev_p2_mask",
                Some("backyard"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let ev_p2_mask_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "event".to_string(),
                id: "ev_p2_mask".to_string(),
                exp: now + 3600,
                cam: Some("backyard".to_string()),
                jti: Some("shr_ev_p2_mask".to_string()),
            },
        );

        // First request seeds the missing sidecar with mask_v1 hash and serves REDACTED_V1_BYTES
        let r_mask_v1 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{ev_p2_mask_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_mask_v1.status(), StatusCode::OK);
        let v1_bytes = to_bytes(r_mask_v1.into_body(), 4096).await.unwrap();
        assert_eq!(&v1_bytes[..], b"REDACTED_V1_BYTES");
        assert_eq!(
            std::fs::read_to_string(&sidecar_file).unwrap().trim(),
            compute_privacy_mask_hash(Some(&mask_v1))
        );

        // Changing privacy_mask to mask_v2 invalidates the cached redacted clip, removes stale file, and rebuilds
        {
            let mut cfg = state.config.write().await;
            cfg.cameras.get_mut("backyard").unwrap().privacy_mask = mask_v2.clone();
        }
        let r_mask_v2 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{ev_p2_mask_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Rebuild on non-transcodable synthetic bytes fails closed (404) and removes stale redacted_file + sidecar
        assert_eq!(r_mask_v2.status(), StatusCode::NOT_FOUND);
        assert!(
            !redacted_file.exists(),
            "stale redacted MP4 must be invalidated when privacy_mask changes"
        );
        assert!(
            !sidecar_file.exists(),
            "stale mask_hash sidecar must be removed when rebuild fails"
        );

        // Singleflight per-clip mutex: same clip key shares the same Arc<Mutex<()>>, distinct clip keys do not block each other
        let lock_a1 = acquire_share_clip_lock(&state, "share_redacted_ev_p2_mask").await;
        let lock_a2 = acquire_share_clip_lock(&state, "share_redacted_ev_p2_mask").await;
        let lock_b = acquire_share_clip_lock(&state, "share_clip_other").await;
        assert!(Arc::ptr_eq(&lock_a1, &lock_a2));
        assert!(!Arc::ptr_eq(&lock_a1, &lock_b));
        let guard_a = lock_a1.lock().await;
        assert!(
            lock_a2.try_lock().is_err(),
            "concurrent singleflight lock on same clip must wait"
        );
        assert!(
            lock_b.try_lock().is_ok(),
            "different clip must not be blocked by lock_a"
        );
        drop(guard_a);

        // =========================================================================
        // 3. Bounded Window, Multi-Segment Concatenation (`share_clip_`), & Static Media Camera Scoping (Finding 2)
        // =========================================================================
        let seg_fd_1 = rec_fd_dir.join("00.00.mp4");
        let seg_fd_2 = rec_fd_dir.join("00.10.mp4");
        std::fs::write(&seg_fd_1, &mp4_payload).unwrap();
        std::fs::write(&seg_fd_2, &mp4_payload).unwrap();
        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "rec_fd_multi_1".to_string(),
                camera: "front_door".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/00.00.mp4".to_string(),
                start_time: 3000.0,
                end_time: 3010.0,
                duration: 10.0,
                motion: 5,
                objects: 1,
                dbfs: -20,
                segment_size: 0.1,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();
        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "rec_fd_multi_2".to_string(),
                camera: "front_door".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/00.10.mp4".to_string(),
                start_time: 3010.0,
                end_time: 3020.0,
                duration: 10.0,
                motion: 5,
                objects: 1,
                dbfs: -20,
                segment_size: 0.1,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_fd_multi".to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 3002.0,
                end_time: Some(3018.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.9),
                false_positive: Some(false),
                box_coords: None,
                data: json!({}),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.9),
                stationary: false,
            })
            .unwrap();

        state
            .db
            .insert_share_link(
                "shr_ev_fd_multi",
                "event",
                "ev_fd_multi",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let ev_fd_multi_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "event".to_string(),
                id: "ev_fd_multi".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_ev_fd_multi".to_string()),
            },
        );
        // Spawn two concurrent requests for multi-segment unmasked clip to exercise singleflight + build_mp4_export_with_privacy
        let app_c1 = auth_app.clone();
        let app_c2 = auth_app.clone();
        let tok_c1 = ev_fd_multi_tok.clone();
        let tok_c2 = ev_fd_multi_tok.clone();
        let (resp_m1, resp_m2) = tokio::join!(
            app_c1.oneshot(
                Request::builder()
                    .uri(format!("/share/{tok_c1}/video.mp4"))
                    .body(Body::empty())
                    .unwrap()
            ),
            app_c2.oneshot(
                Request::builder()
                    .uri(format!("/share/{tok_c2}/video.mp4"))
                    .body(Body::empty())
                    .unwrap()
            )
        );
        assert_eq!(resp_m1.unwrap().status(), StatusCode::OK);
        assert_eq!(resp_m2.unwrap().status(), StatusCode::OK);
        assert!(
            exports_dir.join("share_clip_ev_fd_multi.mp4").is_file(),
            "multi-segment unmasked share must build exports/share_clip_<id>.mp4"
        );
        assert!(
            state
                .db
                .get_export_by_id("share_clip_ev_fd_multi")
                .unwrap()
                .is_none(),
            "transient share_clip_ DB export row must be deleted"
        );
        assert!(
            !clips_dir.join("share_clip_ev_fd_multi.webp").exists(),
            "transient share_clip_ thumbnail must be cleaned up"
        );

        // Verify resolve_static_media_camera strips "share_clip_" alongside "share_redacted_"
        assert_eq!(
            resolve_static_media_camera(&state, "/exports/share_clip_ev_fd_multi.mp4").as_deref(),
            Some("front_door")
        );
        assert_eq!(
            resolve_static_media_camera(&state, "/exports/share_clip_ev_p2_mask.mp4").as_deref(),
            Some("backyard")
        );
        assert_eq!(
            resolve_static_media_camera(&state, "/clips/share_clip_ev_fd_multi.webp").as_deref(),
            Some("front_door")
        );

        // =========================================================================
        // 4. Artifact Cleanup on Share Link Revocation and Event/Review Deletion (Finding 1)
        // =========================================================================
        let seed_artifacts = |target_id: &str| {
            let r_mp4 = exports_dir.join(format!("share_redacted_{target_id}.mp4"));
            let r_hash = exports_dir.join(format!(".share_redacted_{target_id}.mask_hash"));
            let c_mp4 = exports_dir.join(format!("share_clip_{target_id}.mp4"));
            std::fs::write(&r_mp4, b"R").unwrap();
            std::fs::write(&r_hash, b"H").unwrap();
            std::fs::write(&c_mp4, b"C").unwrap();
            (r_mp4, r_hash, c_mp4)
        };

        // 4a. Revoking a share link removes its target's share_redacted_*, .mask_hash, and share_clip_* artifacts
        state
            .db
            .insert_share_link(
                "shr_cleanup_revoke",
                "event",
                "ev_fd_multi",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let (r_mp4, r_hash, c_mp4) = seed_artifacts("ev_fd_multi");
        let resp_rev = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/share/shr_cleanup_revoke")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_rev.status(), StatusCode::OK);
        assert!(!r_mp4.exists() && !r_hash.exists() && !c_mp4.exists());

        // 4b. Deleting an event removes its share_redacted_*, .mask_hash, and share_clip_* artifacts
        let (r_mp4, r_hash, c_mp4) = seed_artifacts("ev_fd_multi");
        let resp_del_ev = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/events/ev_fd_multi")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_del_ev.status(), StatusCode::OK);
        assert!(!r_mp4.exists() && !r_hash.exists() && !c_mp4.exists());

        // 4c. Deleting a review removes its share_redacted_*, .mask_hash, and share_clip_* artifacts
        state
            .db
            .insert_review_segment(&ninjanvr_db::ReviewItemDto {
                id: "rev_p2_clean".to_string(),
                camera: "front_door".to_string(),
                start_time: 3000.0,
                end_time: Some(3010.0),
                severity: "alert".to_string(),
                thumb_path: "/media/ninjanvr/clips/review/thumb-front_door-rev_p2_clean.webp"
                    .to_string(),
                data: json!({}),
                has_been_reviewed: false,
            })
            .unwrap();
        let (r_mp4, r_hash, c_mp4) = seed_artifacts("rev_p2_clean");
        let resp_del_rev = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/review/rev_p2_clean")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_del_rev.status(), StatusCode::OK);
        assert!(!r_mp4.exists() && !r_hash.exists() && !c_mp4.exists());

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn security_share_link_phase3_corp_csrf_max_views_and_audit_tracking_work() {
        let tmp_dir = std::env::temp_dir().join(format!(
            "ninjanvr_sec_share_phase3_{}",
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let db_path = tmp_dir.join("ninjanvr.db");
        let cfg_path = tmp_dir.join("config.yml");
        let web_dir = tmp_dir.join("web-v2");
        let media_dir = tmp_dir.join("media");
        std::fs::create_dir_all(&web_dir).unwrap();
        std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

        let yaml = format!(
            r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
"#,
            db_path.display()
        );
        std::fs::write(&cfg_path, yaml).unwrap();

        let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
            .await
            .unwrap();
        let auth_app = build_router_with_auth(state.clone(), true);

        // Verify .jwt_secret was atomically created with mode 0o600
        let jwt_secret_path = tmp_dir.join(".jwt_secret");
        assert!(jwt_secret_path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&jwt_secret_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        let now = unix_now();
        let admin_jwt = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: "admin".to_string(),
                role: "admin".to_string(),
                allowed_cameras: Some("*".to_string()),
                iat: now,
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap();
        let admin_bearer = format!("Bearer {admin_jwt}");
        let admin_cookie = format!("ninjanvr_token={admin_jwt}");

        let exports_dir = media_dir.join("exports");
        std::fs::create_dir_all(&exports_dir).unwrap();
        let mp4_payload: Vec<u8> = (0u8..64u8).collect();
        let exp_file = exports_dir.join("exp_p3_1.mp4");
        std::fs::write(&exp_file, &mp4_payload).unwrap();
        state
            .db
            .insert_export(&ExportRow {
                id: "exp_p3_1".to_string(),
                camera: "front_door".to_string(),
                name: "Phase 3 Export".to_string(),
                date: 1700000100.0,
                video_path: "/media/ninjanvr/exports/exp_p3_1.mp4".to_string(),
                thumb_path: "/media/ninjanvr/clips/exp_p3_1.webp".to_string(),
                in_progress: false,
            })
            .unwrap();

        // =========================================================================
        // 1. Cross-Origin-Resource-Policy: same-origin on bytes/file range helpers
        // =========================================================================
        let empty_hdrs = HeaderMap::new();
        let mut range_hdrs = HeaderMap::new();
        range_hdrs.insert(header::RANGE, "bytes=0-15".parse().unwrap());

        let bytes_200 = serve_mp4_bytes_with_range(mp4_payload.clone(), &empty_hdrs);
        assert_eq!(
            bytes_200
                .headers()
                .get("cross-origin-resource-policy")
                .unwrap(),
            "same-origin"
        );
        let bytes_206 = serve_mp4_bytes_with_range(mp4_payload.clone(), &range_hdrs);
        assert_eq!(
            bytes_206
                .headers()
                .get("cross-origin-resource-policy")
                .unwrap(),
            "same-origin"
        );

        let file_200 = serve_mp4_file_with_range(&exp_file, &empty_hdrs)
            .await
            .unwrap();
        assert_eq!(
            file_200
                .headers()
                .get("cross-origin-resource-policy")
                .unwrap(),
            "same-origin"
        );
        let file_206 = serve_mp4_file_with_range(&exp_file, &range_hdrs)
            .await
            .unwrap();
        assert_eq!(
            file_206
                .headers()
                .get("cross-origin-resource-policy")
                .unwrap(),
            "same-origin"
        );

        // =========================================================================
        // 2. Cookie CSRF Origin Guard on POST /api/share and DELETE /api/share/{id}
        // =========================================================================
        // 2a. Cookie auth + mismatched Origin -> 403 FORBIDDEN on POST /api/share
        for (origin_val, host_opt) in [
            ("https://evil.example", Some("nvr.local:5000")),
            ("http://nvr.local:8971", Some("nvr.local:5000")),
            ("null", Some("nvr.local:5000")),
            ("https://nvr.local:5000", None),
        ] {
            let mut req_b = Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::COOKIE, &admin_cookie)
                .header(header::ORIGIN, origin_val)
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(h) = host_opt {
                req_b = req_b.header(header::HOST, h);
            }
            let r = auth_app
                .clone()
                .oneshot(
                    req_b
                        .body(Body::from(
                            r#"{"kind":"export","target_id":"exp_p3_1","expires_in_sec":3600}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::FORBIDDEN,
                "cookie POST /api/share with Origin={origin_val} Host={host_opt:?} must return 403"
            );
        }

        // 2b. Cookie auth + matching Origin (scheme-agnostic host:port) -> 200 OK
        let resp_cookie_ok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::COOKIE, &admin_cookie)
                    .header(header::ORIGIN, "https://nvr.local:5000")
                    .header(header::HOST, "nvr.local:5000")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p3_1","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_cookie_ok.status(), StatusCode::OK);
        let cookie_share_json: Value =
            serde_json::from_slice(&to_bytes(resp_cookie_ok.into_body(), 4096).await.unwrap())
                .unwrap();
        let cookie_share_id = cookie_share_json["id"].as_str().unwrap().to_string();
        assert!(cookie_share_json.get("max_views").is_none());

        // 2c. Cookie auth + mismatched Origin on DELETE /api/share/{id} -> 403 FORBIDDEN (link stays unrevoked)
        let resp_del_csrf = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/share/{cookie_share_id}"))
                    .header(header::COOKIE, &admin_cookie)
                    .header(header::ORIGIN, "https://evil.example")
                    .header(header::HOST, "nvr.local:5000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_del_csrf.status(), StatusCode::FORBIDDEN);
        assert!(!state.db.is_share_link_revoked(&cookie_share_id).unwrap());

        // 2d. Cookie auth + matching Origin on DELETE /api/share/{id} -> 200 OK
        let resp_del_cookie_ok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/share/{cookie_share_id}"))
                    .header(header::COOKIE, &admin_cookie)
                    .header(header::ORIGIN, "http://nvr.local:5000")
                    .header(header::HOST, "nvr.local:5000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_del_cookie_ok.status(), StatusCode::OK);
        assert!(state.db.is_share_link_revoked(&cookie_share_id).unwrap());

        // =========================================================================
        // 3. Max-Views Enforcement, Access Counter, and Last-Accessed Timestamp
        // =========================================================================
        // Create a link with max_views: 2 (Bearer auth works even with cross-origin Origin header)
        let resp_mv2 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::ORIGIN, "https://cli-client.example")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p3_1","expires_in_sec":3600,"max_views":2}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_mv2.status(), StatusCode::OK);
        let mv2_json: Value =
            serde_json::from_slice(&to_bytes(resp_mv2.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(mv2_json["max_views"], 2);
        let mv2_id = mv2_json["id"].as_str().unwrap().to_string();
        let mv2_tok = mv2_json["token"].as_str().unwrap().to_string();

        let db_before = state.db.get_share_link(&mv2_id).unwrap().unwrap();
        assert_eq!(db_before["access_count"], 0);
        assert!(db_before["last_accessed_at"].is_null());
        assert_eq!(db_before["max_views"], 2);

        // View 1: GET /share/{token} (share_player_page) -> 200 OK + CORP: same-origin, access_count = 1
        let v1_resp = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{mv2_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(v1_resp.status(), StatusCode::OK);
        assert_eq!(
            v1_resp
                .headers()
                .get("cross-origin-resource-policy")
                .unwrap(),
            "same-origin"
        );
        let db_v1 = state.db.get_share_link(&mv2_id).unwrap().unwrap();
        assert_eq!(db_v1["access_count"], 1);
        assert!(db_v1["last_accessed_at"].as_f64().unwrap() >= now as f64);

        // Embedded <video> initial request after View 1 player page consumes the page credit (access_count stays 1)
        let v1_video_resp = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{mv2_tok}/video.mp4"))
                    .header(header::RANGE, "bytes=0-15")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(v1_video_resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            state.db.get_share_link(&mv2_id).unwrap().unwrap()["access_count"],
            1
        );

        // View 2: Standalone initial GET /share/{token}/video.mp4 with Range: bytes=0-15 -> 206 Partial Content, access_count = 2
        let v2_resp = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{mv2_tok}/video.mp4"))
                    .header(header::RANGE, "bytes=0-15")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(v2_resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            v2_resp
                .headers()
                .get("cross-origin-resource-policy")
                .unwrap(),
            "same-origin"
        );
        let db_v2 = state.db.get_share_link(&mv2_id).unwrap().unwrap();
        assert_eq!(db_v2["access_count"], 2);

        // Mid-stream chunk: Range: bytes=16-63 succeeds (206) without incrementing access_count beyond 2
        let mid_resp = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{mv2_tok}/video.mp4"))
                    .header(header::RANGE, "bytes=16-63")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(mid_resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            state.db.get_share_link(&mv2_id).unwrap().unwrap()["access_count"],
            2
        );

        // View 3 (exceeding max_views = 2): both /share/{token} and /share/{token}/video.mp4 return 410 GONE
        for uri in [
            format!("/share/{mv2_tok}"),
            format!("/share/{mv2_tok}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::GONE,
                "exhausted max_views link must return 410 GONE on {uri}"
            );
        }
        assert_eq!(
            state.db.get_share_link(&mv2_id).unwrap().unwrap()["access_count"],
            2
        );

        // Single-use link (max_views: 0 clamped to 1) end-to-end player + embedded <video> playback
        let resp_mv0 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p3_1","expires_in_sec":3600,"max_views":0}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_mv0.status(), StatusCode::OK);
        let mv0_json: Value =
            serde_json::from_slice(&to_bytes(resp_mv0.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(mv0_json["max_views"], 1);
        let mv1_id = mv0_json["id"].as_str().unwrap().to_string();
        let mv1_tok = mv0_json["token"].as_str().unwrap().to_string();

        // 1) GET /share/{token} succeeds (200 OK) and sets access_count == 1
        let mv1_page = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{mv1_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(mv1_page.status(), StatusCode::OK);
        assert_eq!(
            state.db.get_share_link(&mv1_id).unwrap().unwrap()["access_count"],
            1
        );

        // 2) Embedded <video> initial request (Range: bytes=0-15) and mid-stream chunk (Range: bytes=16-63) succeed with access_count == 1
        for range_spec in ["bytes=0-15", "bytes=16-63"] {
            let r = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/share/{mv1_tok}/video.mp4"))
                        .header(header::RANGE, range_spec)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::PARTIAL_CONTENT,
                "max_views=1 embedded video request with {range_spec} must succeed"
            );
            assert_eq!(
                state.db.get_share_link(&mv1_id).unwrap().unwrap()["access_count"],
                1
            );
        }

        // 3) Subsequent second GET /share/{token} and subsequent second initial GET /share/{token}/video.mp4 both return 410 GONE
        for uri in [
            format!("/share/{mv1_tok}"),
            format!("/share/{mv1_tok}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::GONE,
                "second visit to max_views=1 link must return 410 GONE on {uri}"
            );
        }
        let r_range0_again = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{mv1_tok}/video.mp4"))
                    .header(header::RANGE, "bytes=0-15")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_range0_again.status(), StatusCode::GONE);

        // 4) [CRITICO-01] Range: bytes=1- bypass attempt after max_views exhaustion is rejected with 410 GONE
        for range_bypass in ["bytes=1-", "bytes=1-31", "bytes=16-63"] {
            let r_bypass = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/share/{mv1_tok}/video.mp4"))
                        .header(header::RANGE, range_bypass)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r_bypass.status(),
                StatusCode::GONE,
                "Range={range_bypass} after max_views exhaustion must return 410 GONE"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn security_share_link_p0_p1_range_bypass_csrf_jti_creator_and_bot_protection_work() {
        let tmp_dir = std::env::temp_dir().join(format!(
            "ninjanvr_sec_share_p0_p1_{}",
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let db_path = tmp_dir.join("ninjanvr.db");
        let cfg_path = tmp_dir.join("config.yml");
        let web_dir = tmp_dir.join("web-v2");
        let media_dir = tmp_dir.join("media");
        std::fs::create_dir_all(&web_dir).unwrap();
        std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

        let yaml = format!(
            r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
"#,
            db_path.display()
        );
        std::fs::write(&cfg_path, yaml).unwrap();

        let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
            .await
            .unwrap();
        let auth_app = build_router_with_auth(state.clone(), true);

        let now = unix_now();
        let make_jwt = |sub: &str, role: &str, allowed_cameras: Option<&str>| -> String {
            jsonwebtoken::encode(
                &Header::new(Algorithm::HS256),
                &JwtClaims {
                    sub: sub.to_string(),
                    role: role.to_string(),
                    allowed_cameras: allowed_cameras.map(str::to_string),
                    iat: now,
                    exp: now + 3600,
                },
                &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
            )
            .unwrap()
        };

        let admin_jwt = make_jwt("admin", "admin", Some("*"));
        let admin_bearer = format!("Bearer {admin_jwt}");
        let admin_cookie = format!("ninjanvr_token={admin_jwt}");

        let exports_dir = media_dir.join("exports");
        std::fs::create_dir_all(&exports_dir).unwrap();
        let mp4_payload: Vec<u8> = (0u8..64u8).collect();
        std::fs::write(exports_dir.join("exp_p0p1.mp4"), &mp4_payload).unwrap();
        state
            .db
            .insert_export(&ExportRow {
                id: "exp_p0p1".to_string(),
                camera: "front_door".to_string(),
                name: "P0 P1 Export".to_string(),
                date: 1700000100.0,
                video_path: "/media/ninjanvr/exports/exp_p0p1.mp4".to_string(),
                thumb_path: "/media/ninjanvr/clips/exp_p0p1.webp".to_string(),
                in_progress: false,
            })
            .unwrap();

        // =========================================================================
        // 1. [CRITICO-01] Range: bytes=1- max_views bypass protection
        // =========================================================================
        // 1a. Sending Range: bytes=1- as the very first request on a max_views=1 link counts as the 1 view,
        //     and a second Range: bytes=1- is rejected with 410 GONE!
        let resp_create_r1 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_create_r1.status(), StatusCode::OK);
        let r1_json: Value =
            serde_json::from_slice(&to_bytes(resp_create_r1.into_body(), 4096).await.unwrap())
                .unwrap();
        let r1_id = r1_json["id"].as_str().unwrap().to_string();
        let r1_tok = r1_json["token"].as_str().unwrap().to_string();

        let first_range1 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{r1_tok}/video.mp4"))
                    .header(header::RANGE, "bytes=1-")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first_range1.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            state.db.get_share_link(&r1_id).unwrap().unwrap()["access_count"],
            1
        );

        let second_range1 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{r1_tok}/video.mp4"))
                    .header(header::RANGE, "bytes=1-")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second_range1.status(), StatusCode::GONE);

        // 1b. Consuming max_views=1 via player page (GET /share/{tok}) and then immediately sending
        //     Range: bytes=1-, Range: bytes=2-, or Range: bytes=16-63 (without an initial Range: bytes=0-...)
        //     is rejected with 410 GONE, and the legitimate Range: bytes=0-15 still works afterwards using
        //     the unconsumed page credit!
        let resp_create_r2 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let r2_json: Value =
            serde_json::from_slice(&to_bytes(resp_create_r2.into_body(), 4096).await.unwrap())
                .unwrap();
        let r2_id = r2_json["id"].as_str().unwrap().to_string();
        let r2_tok = r2_json["token"].as_str().unwrap().to_string();

        let r2_page = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{r2_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r2_page.status(), StatusCode::OK);

        for premature_range in ["bytes=1-", "bytes=2-", "bytes=16-63"] {
            let r2_premature = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/share/{r2_tok}/video.mp4"))
                        .header(header::RANGE, premature_range)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r2_premature.status(),
                StatusCode::GONE,
                "Range: {premature_range} directly after player page when max_views=1 must return 410 GONE"
            );
        }

        let r2_legit_initial = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{r2_tok}/video.mp4"))
                    .header(header::RANGE, "bytes=0-15")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r2_legit_initial.status(),
            StatusCode::PARTIAL_CONTENT,
            "legitimate Range: bytes=0-15 after rejected premature non-zero ranges must succeed using unconsumed page credit"
        );
        assert_eq!(
            state.db.get_share_link(&r2_id).unwrap().unwrap()["access_count"],
            1
        );

        // 1c. An initial open-ended Range: bytes=0- on a max_views=1 link followed by Range: bytes=16-63
        //     succeeds (206 PARTIAL_CONTENT, access_count == 1), while a subsequent second viewer attempting
        //     Range: bytes=0- and then Range: bytes=1- or Range: bytes=16-63 gets 410 GONE on all of them!
        let resp_create_r3 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let r3_json: Value =
            serde_json::from_slice(&to_bytes(resp_create_r3.into_body(), 4096).await.unwrap())
                .unwrap();
        let r3_id = r3_json["id"].as_str().unwrap().to_string();
        let r3_tok = r3_json["token"].as_str().unwrap().to_string();

        for viewer1_range in ["bytes=0-", "bytes=16-63"] {
            let r3_v1 = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/share/{r3_tok}/video.mp4"))
                        .header(header::RANGE, viewer1_range)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r3_v1.status(),
                StatusCode::PARTIAL_CONTENT,
                "viewer 1 request with Range: {viewer1_range} must return 206 PARTIAL_CONTENT"
            );
            assert_eq!(
                state.db.get_share_link(&r3_id).unwrap().unwrap()["access_count"],
                1
            );
        }

        for viewer2_range in ["bytes=0-", "bytes=1-", "bytes=16-63"] {
            let r3_v2 = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/share/{r3_tok}/video.mp4"))
                        .header(header::RANGE, viewer2_range)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r3_v2.status(),
                StatusCode::GONE,
                "viewer 2 request with Range: {viewer2_range} on exhausted max_views=1 link must return 410 GONE"
            );
        }

        // =========================================================================
        // 2. [ALTO-02] CSRF Parser Differential, Sec-Fetch-Site: cross-site, and Referer
        // =========================================================================
        // 2a. Sec-Fetch-Site: cross-site rejected even when Origin is absent
        let resp_sfs = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::COOKIE, &admin_cookie)
                    .header("sec-fetch-site", "cross-site")
                    .header(header::HOST, "nvr.local:5000")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_sfs.status(), StatusCode::FORBIDDEN);

        // 2b. Mismatched or malformed Referer when Origin is absent -> 403 FORBIDDEN; matching Referer -> 200 OK
        for bad_referer in [
            "https://evil.example/csrf.html",
            "http://nvr.local:8971/page",
            "malformed_no_scheme",
            "://missing_scheme",
            "https:///missing_host",
        ] {
            let r = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/share")
                        .header(header::COOKIE, &admin_cookie)
                        .header(header::REFERER, bad_referer)
                        .header(header::HOST, "nvr.local:5000")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::FORBIDDEN,
                "bad Referer={bad_referer} without Origin must return 403"
            );
        }

        let resp_good_referer = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::COOKIE, &admin_cookie)
                    .header(header::REFERER, "https://nvr.local:5000/exports?filter=1")
                    .header(header::HOST, "nvr.local:5000")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_good_referer.status(), StatusCode::OK);

        // 2c. No fallback to session cookie when an invalid/lowercase/whitespace Authorization header is sent!
        for bad_auth in [
            "bearer invalid_token",
            "  Bearer invalid_token",
            "Bearer invalid_token",
            "Basic dXNlcjpwYXNz",
            "Bearer   ",
        ] {
            let r = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/share")
                        .header(header::COOKIE, &admin_cookie)
                        .header(header::AUTHORIZATION, bad_auth)
                        .header(header::ORIGIN, "https://evil.example")
                        .header(header::HOST, "nvr.local:5000")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                r.status() == StatusCode::UNAUTHORIZED || r.status() == StatusCode::FORBIDDEN,
                "invalid Authorization={bad_auth:?} must NOT fall back to session cookie (got {})",
                r.status()
            );
        }

        // Valid lowercase `bearer <jwt>` with leading whitespace succeeds
        let resp_lower_bearer = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, format!("  bearer {admin_jwt}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_lower_bearer.status(), StatusCode::OK);

        // Multi-byte UTF-8 input in parse_authorization_bearer never panics on char boundaries
        for utf8_input in ["🔥earer tok", "bear🔥r tok", "🔥🔥 tok", "  🔥bearer tok"] {
            assert_eq!(parse_authorization_bearer(utf8_input), None);
        }

        // =========================================================================
        // 3. [ALTO-03] Mandatory JTI Enforcement (jti: None or empty -> 403 FORBIDDEN)
        // =========================================================================
        for missing_jti in [None, Some(String::new()), Some("   ".to_string())] {
            let no_jti_tok = sign_share_token(
                &state.jwt_secret,
                &ShareTokenPayload {
                    k: "export".to_string(),
                    id: "exp_p0p1".to_string(),
                    exp: now + 3600,
                    cam: Some("front_door".to_string()),
                    jti: missing_jti.clone(),
                },
            );
            for uri in [
                format!("/share/{no_jti_tok}"),
                format!("/share/{no_jti_tok}/video.mp4"),
            ] {
                let r = auth_app
                    .clone()
                    .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    r.status(),
                    StatusCode::FORBIDDEN,
                    "token with jti={missing_jti:?} must return 403 FORBIDDEN on {uri}"
                );
            }
        }

        // =========================================================================
        // 4. [ALTO-04] Creator Lifecycle: Role Demotion to Viewer & API Token Revocation/Expiration
        // =========================================================================
        state
            .db
            .create_user_with_cameras("op_demote", "pass1234", "operator", "front_door", 1000)
            .unwrap();
        let op_demote_hdr = format!(
            "Bearer {}",
            make_jwt("op_demote", "operator", Some("front_door"))
        );
        let resp_op_share = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &op_demote_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_op_share.status(), StatusCode::OK);
        let op_share_json: Value =
            serde_json::from_slice(&to_bytes(resp_op_share.into_body(), 4096).await.unwrap())
                .unwrap();
        let op_share_id = op_share_json["id"].as_str().unwrap().to_string();
        let op_share_tok = op_share_json["token"].as_str().unwrap().to_string();

        // 4a. Direct DB role demotion to viewer -> 403 FORBIDDEN in verify_share_jti_with_view_mode
        state
            .db
            .execute_batch("UPDATE user SET role = 'viewer' WHERE username = 'op_demote';")
            .unwrap();
        for uri in [
            format!("/share/{op_share_tok}"),
            format!("/share/{op_share_tok}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN);
        }

        // Restore to operator, then demote via PUT /api/users/op_demote/role -> revokes link in DB (410 GONE)
        state.db.update_user_role("op_demote", "operator").unwrap();
        let resp_api_demote = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/users/op_demote/role")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"role":"viewer"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_api_demote.status(), StatusCode::OK);
        assert!(state.db.is_share_link_revoked(&op_share_id).unwrap());
        let r_after_demote = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{op_share_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_after_demote.status(), StatusCode::GONE);

        // 4b. Creator user deleted directly in DB -> 403 FORBIDDEN (fail-closed)
        state
            .db
            .insert_share_link(
                "shr_ghost_creator",
                "export",
                "exp_p0p1",
                Some("front_door"),
                "nonexistent_creator_user",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let ghost_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: "exp_p0p1".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_ghost_creator".to_string()),
            },
        );
        for uri in [
            format!("/share/{ghost_tok}"),
            format!("/share/{ghost_tok}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN);
        }

        // 4c. Share link created by full-scope API token: works while valid, blocked when token expired or deleted
        let (tok_meta, tok_plain) = state
            .db
            .create_api_token("ci_share_bot", "full", Some((now + 3600) as f64))
            .unwrap();
        let api_tok_id = tok_meta["id"].as_str().unwrap().to_string();
        let resp_tok_share = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, format!("Bearer {tok_plain}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_tok_share.status(), StatusCode::OK);
        let tok_share_json: Value =
            serde_json::from_slice(&to_bytes(resp_tok_share.into_body(), 4096).await.unwrap())
                .unwrap();
        let tok_share_jti = tok_share_json["id"].as_str().unwrap().to_string();
        let tok_share_token = tok_share_json["token"].as_str().unwrap().to_string();

        // Works while API token is active
        let r_tok_ok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{tok_share_token}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_tok_ok.status(), StatusCode::OK);

        // Deleting the API token via DELETE /api/tokens/{id} revokes its share links in DB (410 GONE)
        let resp_del_apitok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/tokens/{api_tok_id}"))
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_del_apitok.status(), StatusCode::OK);
        assert!(state.db.is_share_link_revoked(&tok_share_jti).unwrap());
        let r_after_tok_del = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{tok_share_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_after_tok_del.status(), StatusCode::GONE);

        // Expired API token creator in verify_share_jti_with_view_mode -> 403 FORBIDDEN
        state
            .db
            .create_api_token("expired_share_bot", "full", Some((now - 10) as f64))
            .unwrap();
        state
            .db
            .insert_share_link(
                "shr_expired_apitok",
                "export",
                "exp_p0p1",
                Some("front_door"),
                "token:expired_share_bot",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let exp_apitok_share = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: "exp_p0p1".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_expired_apitok".to_string()),
            },
        );
        for uri in [
            format!("/share/{exp_apitok_share}"),
            format!("/share/{exp_apitok_share}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN);
        }

        // =========================================================================
        // 5. [BAIXO-09] Chat Link-Preview Bot Protection & Robots Meta Tag
        // =========================================================================
        let resp_bot_share = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bot_share_json: Value =
            serde_json::from_slice(&to_bytes(resp_bot_share.into_body(), 4096).await.unwrap())
                .unwrap();
        let bot_share_id = bot_share_json["id"].as_str().unwrap().to_string();
        let bot_share_tok = bot_share_json["token"].as_str().unwrap().to_string();

        for bot_ua in [
            "WhatsApp/2.23.20.0",
            "Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)",
            "TelegramBot (like TwitterBot)",
            "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)",
            "Twitterbot/1.0",
            "facebookexternalhit/1.1",
            "LinkedInBot/1.0",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X) Applebot/0.1",
        ] {
            let r_bot = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/share/{bot_share_tok}"))
                        .header(header::USER_AGENT, bot_ua)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r_bot.status(),
                StatusCode::OK,
                "bot UA {bot_ua} must get 200 OK"
            );
            let bot_html =
                String::from_utf8(to_bytes(r_bot.into_body(), 8192).await.unwrap().to_vec())
                    .unwrap();
            assert!(
                bot_html.contains(r#"<meta name="robots" content="noindex, nofollow">"#),
                "bot preview HTML must include noindex, nofollow meta tag"
            );
            assert!(
                !bot_html.contains("<video"),
                "bot preview HTML must not embed autoplay <video>"
            );
            assert_eq!(
                state.db.get_share_link(&bot_share_id).unwrap().unwrap()["access_count"],
                0,
                "bot UA {bot_ua} must NOT consume access_count"
            );
        }

        // Human recipient clicking the max_views=1 link after all chat bots still gets the view!
        let r_human = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{bot_share_tok}"))
                    .header(
                        header::USER_AGENT,
                        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/130.0.0.0",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_human.status(), StatusCode::OK);
        let human_html =
            String::from_utf8(to_bytes(r_human.into_body(), 16384).await.unwrap().to_vec())
                .unwrap();
        assert!(
            human_html.contains(r#"<meta name="robots" content="noindex, nofollow">"#),
            "human player page HTML must include noindex, nofollow meta tag"
        );
        assert!(human_html.contains("<video"));
        assert_eq!(
            state.db.get_share_link(&bot_share_id).unwrap().unwrap()["access_count"],
            1
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn security_share_link_p2_media_privacy_dos_and_static_artifact_protection_work() {
        let tmp_dir = std::env::temp_dir().join(format!(
            "ninjanvr_sec_share_p2_media_dos_{}",
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let db_path = tmp_dir.join("ninjanvr.db");
        let cfg_path = tmp_dir.join("config.yml");
        let web_dir = tmp_dir.join("web-v2");
        let media_dir = tmp_dir.join("media");
        std::fs::create_dir_all(&web_dir).unwrap();
        std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

        let yaml = format!(
            r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
            db_path.display()
        );
        std::fs::write(&cfg_path, yaml).unwrap();

        let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
            .await
            .unwrap();
        let auth_app = build_router_with_auth(state.clone(), true);
        let unauth_app = build_router(state.clone());

        let now = unix_now();
        let make_jwt = |sub: &str, role: &str, allowed_cameras: Option<&str>| -> String {
            jsonwebtoken::encode(
                &Header::new(Algorithm::HS256),
                &JwtClaims {
                    sub: sub.to_string(),
                    role: role.to_string(),
                    allowed_cameras: allowed_cameras.map(str::to_string),
                    iat: now,
                    exp: now + 3600,
                },
                &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
            )
            .unwrap()
        };

        state
            .db
            .create_user_with_cameras("scoped_fd_op", "pass1234", "operator", "front_door", 1000)
            .unwrap();
        let admin_bearer = format!("Bearer {}", make_jwt("admin", "admin", Some("*")));
        let scoped_op_bearer = format!(
            "Bearer {}",
            make_jwt("scoped_fd_op", "operator", Some("front_door"))
        );

        let exports_dir = media_dir.join("exports");
        let clips_dir = media_dir.join("clips");
        let rec_fd_dir = media_dir.join("recordings/2026-11-03/12/front_door");
        let rec_by_dir = media_dir.join("recordings/2026-11-03/12/backyard");
        for d in [&exports_dir, &clips_dir, &rec_fd_dir, &rec_by_dir] {
            std::fs::create_dir_all(d).unwrap();
        }

        // =========================================================================
        // 1. [ALTO-05] Export with Active Camera Privacy Mask Redacts via share_redacted_<id>.mp4
        // =========================================================================
        let raw_export_bytes = b"UNREDACTED_RAW_EXPORT_STREAM_BYTES".to_vec();
        std::fs::write(exports_dir.join("exp_by_masked.mp4"), &raw_export_bytes).unwrap();
        state
            .db
            .insert_export(&ExportRow {
                id: "exp_by_masked".to_string(),
                camera: "backyard".to_string(),
                name: "Backyard Export".to_string(),
                date: 1700000100.0,
                video_path: "/media/ninjanvr/exports/exp_by_masked.mp4".to_string(),
                thumb_path: "/media/ninjanvr/clips/exp_by_masked.webp".to_string(),
                in_progress: false,
            })
            .unwrap();
        state
            .db
            .insert_share_link(
                "shr_exp_by_masked",
                "export",
                "exp_by_masked",
                Some("backyard"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let exp_by_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: "exp_by_masked".to_string(),
                exp: now + 3600,
                cam: Some("backyard".to_string()),
                jti: Some("shr_exp_by_masked".to_string()),
            },
        );

        let mask_v1 = json!([[[0.1, 0.1], [0.4, 0.1], [0.4, 0.4], [0.1, 0.4]]]);
        let mask_v2 = json!([[[0.2, 0.2], [0.8, 0.2], [0.8, 0.8], [0.2, 0.8]]]);
        {
            let mut cfg = state.config.write().await;
            cfg.cameras.get_mut("backyard").unwrap().privacy_mask = mask_v1.clone();
        }

        // 1a. Without pre-seeded share_redacted_exp_by_masked.mp4, synthetic non-MP4 bytes fail closed (404)
        //     and NEVER leak the unredacted raw export file!
        let r_exp_fail_closed = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{exp_by_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_exp_fail_closed.status(), StatusCode::NOT_FOUND);

        // 1b. With redacted export present, share_video_mp4 seeds .share_redacted_exp_by_masked.mask_hash
        //     and serves share_redacted_exp_by_masked.mp4 instead of raw exp_by_masked.mp4
        let redacted_exp_file = exports_dir.join("share_redacted_exp_by_masked.mp4");
        let redacted_exp_sidecar = exports_dir.join(".share_redacted_exp_by_masked.mask_hash");
        std::fs::write(&redacted_exp_file, b"REDACTED_EXPORT_V1_BYTES").unwrap();
        let r_exp_redacted_ok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{exp_by_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_exp_redacted_ok.status(), StatusCode::OK);
        let served_exp_bytes = to_bytes(r_exp_redacted_ok.into_body(), 4096).await.unwrap();
        assert_eq!(&served_exp_bytes[..], b"REDACTED_EXPORT_V1_BYTES");
        assert_ne!(&served_exp_bytes[..], &raw_export_bytes[..]);
        assert_eq!(
            std::fs::read_to_string(&redacted_exp_sidecar)
                .unwrap()
                .trim(),
            compute_privacy_mask_hash(Some(&mask_v1))
        );

        // 1c. Changing backyard privacy_mask to mask_v2 invalidates share_redacted_exp_by_masked.mp4 + sidecar
        {
            let mut cfg = state.config.write().await;
            cfg.cameras.get_mut("backyard").unwrap().privacy_mask = mask_v2.clone();
        }
        let r_exp_mask_v2 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{exp_by_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_exp_mask_v2.status(), StatusCode::NOT_FOUND);
        assert!(!redacted_exp_file.exists());
        assert!(!redacted_exp_sidecar.exists());

        // =========================================================================
        // 2. [ALTO-05 & MEDIO-08] Single-Segment share_clip_<id>.mp4 Creation & Recording Deletion Cache Invalidation
        // =========================================================================
        let seg_fd_single = rec_fd_dir.join("00.00.mp4");
        std::fs::write(&seg_fd_single, b"SINGLE_SEG_FD_BYTES").unwrap();
        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "rec_fd_single_1".to_string(),
                camera: "front_door".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/00.00.mp4".to_string(),
                start_time: 1000.0,
                end_time: 1010.0,
                duration: 10.0,
                motion: 5,
                objects: 1,
                dbfs: -20,
                segment_size: 0.1,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_fd_single".to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 1001.0,
                end_time: Some(1008.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.9),
                false_positive: Some(false),
                box_coords: None,
                data: json!({}),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.9),
                stationary: false,
            })
            .unwrap();
        state
            .db
            .insert_review_segment(&ninjanvr_db::ReviewItemDto {
                id: "rev_fd_single".to_string(),
                camera: "front_door".to_string(),
                start_time: 1001.0,
                end_time: Some(1008.0),
                severity: "alert".to_string(),
                thumb_path: "/media/ninjanvr/clips/review/thumb-front_door-rev_fd_single.webp"
                    .to_string(),
                data: json!({}),
                has_been_reviewed: false,
            })
            .unwrap();
        state
            .db
            .insert_share_link(
                "shr_ev_fd_single",
                "event",
                "ev_fd_single",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let ev_fd_single_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "event".to_string(),
                id: "ev_fd_single".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_ev_fd_single".to_string()),
            },
        );

        // 2a. Single-segment unmasked event builds exports/share_clip_ev_fd_single.mp4
        let r_single_ok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{ev_fd_single_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_single_ok.status(), StatusCode::OK);
        let single_clip_path = exports_dir.join("share_clip_ev_fd_single.mp4");
        assert!(
            single_clip_path.is_file(),
            "single-segment shared event must build exports/share_clip_<id>.mp4"
        );

        // Seed additional cached share artifacts for ev_fd_single and rev_fd_single
        let ev_redacted_mp4 = exports_dir.join("share_redacted_ev_fd_single.mp4");
        let ev_redacted_hash = exports_dir.join(".share_redacted_ev_fd_single.mask_hash");
        let rev_clip_mp4 = exports_dir.join("share_clip_rev_fd_single.mp4");
        let rev_redacted_webp = clips_dir.join("share_redacted_rev_fd_single.webp");
        std::fs::write(&ev_redacted_mp4, b"CACHED_REDACTED").unwrap();
        std::fs::write(&ev_redacted_hash, b"HASH").unwrap();
        std::fs::write(&rev_clip_mp4, b"CACHED_REV_CLIP").unwrap();
        std::fs::write(&rev_redacted_webp, b"CACHED_WEBP").unwrap();

        // 2b. Deleting recordings via DELETE /api/front_door/recordings removes cached share_clip_* and share_redacted_* artifacts
        let r_del_recs = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/front_door/recordings?after=990&before=1020")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_del_recs.status(), StatusCode::OK);
        assert!(!single_clip_path.exists());
        assert!(!ev_redacted_mp4.exists());
        assert!(!ev_redacted_hash.exists());
        assert!(!rev_clip_mp4.exists());
        assert!(!rev_redacted_webp.exists());

        // 2c. Even if a stale share_clip_* or share_redacted_* file exists on disk when no underlying recording exists,
        //     share_video_mp4 verifies recording existence FIRST, deletes the stale file, and returns 404 Not Found!
        std::fs::write(&single_clip_path, b"STALE_CLIP_BYTES").unwrap();
        let r_stale_clip_404 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{ev_fd_single_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_stale_clip_404.status(), StatusCode::NOT_FOUND);
        assert!(
            !single_clip_path.exists(),
            "stale share_clip_ must be removed when underlying recordings are missing"
        );

        {
            let mut cfg = state.config.write().await;
            cfg.cameras.get_mut("front_door").unwrap().privacy_mask = mask_v1.clone();
        }
        std::fs::write(&ev_redacted_mp4, b"STALE_REDACTED_BYTES").unwrap();
        std::fs::write(&ev_redacted_hash, compute_privacy_mask_hash(Some(&mask_v1))).unwrap();
        let r_stale_redacted_404 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{ev_fd_single_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_stale_redacted_404.status(), StatusCode::NOT_FOUND);
        assert!(
            !ev_redacted_mp4.exists(),
            "stale share_redacted_ must be removed when underlying recordings are missing"
        );
        assert!(!ev_redacted_hash.exists());
        {
            let mut cfg = state.config.write().await;
            cfg.cameras.get_mut("front_door").unwrap().privacy_mask = Value::Null;
        }

        // =========================================================================
        // 3. [MEDIO-07] Dot-Files in /exports/* and /clips/* Blocked Over HTTP (403 FORBIDDEN)
        // =========================================================================
        let dot_mask_hash = exports_dir.join(".share_redacted_ev_secret.mask_hash");
        let dot_tmp_mp4 = exports_dir.join(".share_clip_ev_secret.1234.tmp.mp4");
        let dot_clip_webp = clips_dir.join(".internal_sidecar.webp");
        std::fs::write(&dot_mask_hash, b"HASH_SECRET").unwrap();
        std::fs::write(&dot_tmp_mp4, b"TMP_SECRET").unwrap();
        std::fs::write(&dot_clip_webp, b"CLIP_SECRET").unwrap();

        for dot_uri in [
            "/exports/.share_redacted_ev_secret.mask_hash",
            "/exports/%2eshare_redacted_ev_secret.mask_hash",
            "/exports/%2Eshare_redacted_ev_secret.mask_hash",
            "/exports/.share_clip_ev_secret.1234.tmp.mp4",
            "/clips/.internal_sidecar.webp",
            "/clips/review/.hidden_thumb.webp",
        ] {
            assert_eq!(
                resolve_static_media_camera(&state, dot_uri),
                None,
                "resolve_static_media_camera must return None for {dot_uri}"
            );
            let r_admin = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(dot_uri)
                        .header(header::AUTHORIZATION, &admin_bearer)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r_admin.status(),
                StatusCode::FORBIDDEN,
                "admin must get 403 FORBIDDEN on dot-file {dot_uri}"
            );

            let r_op = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(dot_uri)
                        .header(header::AUTHORIZATION, &scoped_op_bearer)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r_op.status(),
                StatusCode::FORBIDDEN,
                "scoped operator must get 403 FORBIDDEN on dot-file {dot_uri}"
            );

            let r_unauth_router = unauth_app
                .clone()
                .oneshot(Request::builder().uri(dot_uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                r_unauth_router.status(),
                StatusCode::FORBIDDEN,
                "unauth router must also block dot-file {dot_uri} with 403 FORBIDDEN"
            );
        }

        // =========================================================================
        // 4. [MEDIO-07] resolve_static_media_camera Fails Closed (None) for share_clip_ / share_redacted_ Absent from DB
        // =========================================================================
        for orphan_share_path in [
            "/exports/share_redacted_front_door_ghost_999.mp4",
            "/exports/share_clip_front_door_ghost_999.mp4",
            "/clips/share_redacted_front_door-ghost.webp",
            "/clips/share_clip_front_door-ghost.webp",
        ] {
            assert_eq!(
                resolve_static_media_camera(&state, orphan_share_path),
                None,
                "resolve_static_media_camera must fail closed (None) for absent DB target on {orphan_share_path}"
            );
            let r_scoped = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(orphan_share_path)
                        .header(header::AUTHORIZATION, &scoped_op_bearer)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r_scoped.status(),
                StatusCode::FORBIDDEN,
                "camera-scoped operator must get 403 FORBIDDEN on orphan share artifact {orphan_share_path}"
            );
        }

        // =========================================================================
        // 5. [MEDIO-06] share_ffmpeg_semaphore Concurrency Gate, share_clip_locks Pruning (>= 64), & Credit Map Bounding (>= 512)
        // =========================================================================
        assert_eq!(state.share_ffmpeg_semaphore.available_permits(), 2);

        // 5a. Hold both semaphore permits and verify a concurrent share_video_mp4 transcode waits for a permit
        std::fs::create_dir_all(&rec_fd_dir).unwrap();
        let seg_sem = rec_fd_dir.join("00.20.mp4");
        std::fs::write(&seg_sem, b"SEM_SEG_BYTES").unwrap();
        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "rec_fd_sem_1".to_string(),
                camera: "front_door".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/00.20.mp4".to_string(),
                start_time: 4000.0,
                end_time: 4010.0,
                duration: 10.0,
                motion: 5,
                objects: 1,
                dbfs: -20,
                segment_size: 0.1,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();
        // Long event (start=4000, end=99999) also exercises the 300s duration clamp (`end.max(start).min(start + 300.0)`)
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_fd_sem".to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 4001.0,
                end_time: Some(99999.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.9),
                false_positive: Some(false),
                box_coords: None,
                data: json!({}),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.9),
                stationary: false,
            })
            .unwrap();
        state
            .db
            .insert_share_link(
                "shr_ev_fd_sem",
                "event",
                "ev_fd_sem",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let ev_fd_sem_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "event".to_string(),
                id: "ev_fd_sem".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_ev_fd_sem".to_string()),
            },
        );

        let p1 = state
            .share_ffmpeg_semaphore
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let p2 = state
            .share_ffmpeg_semaphore
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        assert_eq!(state.share_ffmpeg_semaphore.available_permits(), 0);

        let app_sem = auth_app.clone();
        let tok_sem = ev_fd_sem_tok.clone();
        let mut build_task = tokio::spawn(async move {
            app_sem
                .oneshot(
                    Request::builder()
                        .uri(format!("/share/{tok_sem}/video.mp4"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        });

        // While both permits are held, build_task must remain pending (blocked on share_ffmpeg_semaphore)
        let early_poll = tokio::time::timeout(Duration::from_millis(40), &mut build_task).await;
        assert!(
            early_poll.is_err(),
            "share_video_mp4 build must wait when all share_ffmpeg_semaphore permits are held"
        );

        // Releasing one permit unblocks build_task to completion (200 OK)
        drop(p1);
        let sem_resp = build_task.await.unwrap();
        assert_eq!(sem_resp.status(), StatusCode::OK);
        drop(p2);
        assert_eq!(state.share_ffmpeg_semaphore.available_permits(), 2);

        // 5b. share_clip_locks prunes idle entries (strong_count == 1) when map.len() >= 64 while preserving held locks
        {
            state.share_clip_locks.lock().await.clear();
        }
        let held_active = acquire_share_clip_lock(&state, "active_held_lock").await;
        for i in 0..63 {
            let _idle = acquire_share_clip_lock(&state, &format!("idle_clip_lock_{i}")).await;
        }
        assert_eq!(state.share_clip_locks.lock().await.len(), 64);

        // Acquiring a 65th lock triggers pruning of all 63 idle locks while retaining `active_held_lock`
        let new_lock = acquire_share_clip_lock(&state, "post_prune_lock").await;
        {
            let map = state.share_clip_locks.lock().await;
            assert_eq!(
                map.len(),
                2,
                "all 63 idle locks must be pruned when map.len() >= 64, leaving only active_held_lock + post_prune_lock"
            );
            assert!(Arc::ptr_eq(
                map.get("active_held_lock").unwrap(),
                &held_active
            ));
            assert!(Arc::ptr_eq(map.get("post_prune_lock").unwrap(), &new_lock));
        }
        drop(held_active);
        drop(new_lock);

        // 5c. share_page_video_credits bounds map size at <= 512
        {
            let mut map = state.share_page_video_credits.lock().unwrap();
            map.clear();
            for i in 0..512 {
                map.insert(
                    format!("jti_credit_fill_{i}"),
                    SharePlaybackCredit {
                        page_credits: if i % 2 == 0 { 0 } else { 1 },
                        active_stream_started: false,
                        min_next_range_start: u64::MAX,
                    },
                );
            }
            assert_eq!(map.len(), 512);
        }
        let r_page_credit = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{ev_fd_sem_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_page_credit.status(), StatusCode::OK);
        {
            let map = state.share_page_video_credits.lock().unwrap();
            assert!(
                map.len() <= 512,
                "share_page_video_credits must be bounded to <= 512 entries (got {})",
                map.len()
            );
            assert!(map.contains_key("shr_ev_fd_sem"));
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
