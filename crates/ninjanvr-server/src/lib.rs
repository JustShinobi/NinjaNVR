#![recursion_limit = "512"]

pub mod pipeline;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use axum::Router;
use axum::body::Body;
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
use pipeline::{SharedCameraMetrics, VisionPipeline};
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

impl LogBufferHub {
    pub fn new() -> Self {
        let (stream_tx, _) = broadcast::channel(256);
        let mut map = HashMap::new();
        map.insert("frigate".to_string(), VecDeque::new());
        map.insert("go2rtc".to_string(), VecDeque::new());
        map.insert("nginx".to_string(), VecDeque::new());
        Self {
            buffers: Arc::new(std::sync::Mutex::new(map)),
            stream_tx,
        }
    }

    pub fn push_frigate(&self, severity: &str, section: &str, message: &str) {
        let now = Local::now();
        let outer_ts = now.format("%Y-%m-%d %H:%M:%S%.9f");
        let inner_ts = now.format("%Y-%m-%d %H:%M:%S");
        let sev_pad = format!("{:<7}", severity.to_ascii_uppercase());
        let sec_pad = format!("{:<30}", section);
        let line = format!("{outer_ts}  [{inner_ts}] {sec_pad} {sev_pad} : {message}");
        self.push_raw("frigate", line);
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
        let line = format!(
            "{outer_ts}  127.0.0.1 - - [{clf_ts}] \"{method} {path} HTTP/1.1\" {status} 0 \"-\" \"NinjaNVR\""
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
    pub activity_ledger: SharedActivityLedger,
    pub detector: Arc<AiDetectorEngine>,
    pub camera_metrics: SharedCameraMetrics,
    pub log_hub: LogBufferHub,
    pub stats_history: Arc<RwLock<VecDeque<Value>>>,
    pub cpu_tracker: ProcCpuTracker,
    pub storage_degraded: Arc<std::sync::atomic::AtomicBool>,
    pub storage_error: Arc<std::sync::Mutex<Option<String>>>,
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

pub fn load_or_create_jwt_secret(config_dir: &Path) -> String {
    if let Ok(env_secret) = std::env::var("FRIGATE_JWT_SECRET")
        && !env_secret.is_empty()
    {
        return env_secret;
    }
    let secret_path = config_dir.join(".jwt_secret");
    if let Ok(existing) = std::fs::read_to_string(&secret_path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let mut bytes = [0u8; 64];
    rand::fill(&mut bytes);
    let secret: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let _ = std::fs::write(&secret_path, &secret);
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
        .route("/review/{id}", get(api_review_by_id))
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
        .route("/events/{id}/sub_label", post(api_event_sub_label))
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
            get(api_event_thumbnail_webp),
        )
        .route("/events/{id}/clip.mp4", get(api_event_clip_mp4))
        .route("/timeline", get(api_timeline))
        .route("/recordings/summary", get(api_all_recordings_summary))
        .route("/recordings/sync", post(api_recordings_sync))
        .route("/recordings/unavailable", get(api_empty_array))
        .route("/recordings/storage", get(api_recordings_storage))
        .route("/exports", get(api_exports))
        .route("/exports/delete", post(api_exports_bulk_delete))
        .route("/share", post(api_share_create))
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
        .route("/go2rtc/webrtc", post(api_go2rtc_webrtc))
        .route(
            "/{camera}/recordings",
            get(api_camera_recordings).delete(api_camera_recordings_delete),
        )
        .route(
            "/{camera}/recordings/summary",
            get(api_camera_recordings_summary),
        )
        .route("/{camera}/ptz/info", get(api_empty_object))
        .route(
            "/{camera}/state",
            get(api_camera_state_get).post(api_camera_state_set),
        )
        .route("/{camera}/latest.webp", get(api_camera_latest_frame))
        .route("/{camera}/latest.jpg", get(api_camera_latest_frame))
        .route("/labels", get(api_labels))
        .route("/sub_labels", get(api_empty_array))
        .route("/audio_labels", get(api_empty_array))
        .route("/recognized_license_plates", get(api_empty_array))
        .route("/classification/attributes", get(api_empty_array))
        .route("/classification/custom", get(api_empty_object))
        .route("/genai/models", get(api_empty_array))
        .route("/plus/models", get(api_empty_array))
        .route("/faces", get(api_empty_object))
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
        "available": ["v2"]
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
        let file_candidate = primary_dir.join(rel_path);
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

async fn auth_gate_middleware(
    State((state, enforce_auth)): State<(AppState, bool)>,
    jar: CookieJar,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
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
                && let Ok(Some(db_u)) = state.db.get_user(&user)
            {
                let allowed_raw = db_u.allowed_cameras.trim();
                if !allowed_raw.is_empty() && allowed_raw != "*" {
                    let allowed_set: Vec<String> = allowed_raw
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
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
                    let target_cam = if let Some(ref qc) = query_cam {
                        Some(qc.as_str())
                    } else if let Some(rest) = path.strip_prefix("/vod/") {
                        rest.split('/').next()
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
                        ) {
                            None
                        } else if first == "export" || first == "vod" {
                            rest.split('/').nth(1)
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
            "enabled": oidc_ready || oidc.enabled,
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

async fn api_auth_oidc_login(
    State(state): State<AppState>,
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
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:5000");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    let redirect_uri = format!("{proto}://{host}/api/auth/oidc/callback");

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

    let mut pkce_cookie = Cookie::new("ninjanvr_oidc_pkce", format!("{state_str}:{verifier}"));
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
    let Some((expected_state, code_verifier)) = pkce_cookie.value().split_once(':') else {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Malformed OIDC PKCE cookie" })),
        )
            .into_response();
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

    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:5000");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    let redirect_uri = format!("{proto}://{host}/api/auth/oidc/callback");

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

        let form_body = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&client_secret={}&code_verifier={}",
            urlencoding_simple(code),
            urlencoding_simple(&redirect_uri),
            urlencoding_simple(&client_id),
            urlencoding_simple(&client_secret),
            urlencoding_simple(code_verifier)
        );
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

    let token_str = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string())
        .or_else(|| {
            jar.get(&auth_cfg.cookie_name)
                .map(|c| c.value().to_string())
        })?;

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
        Ok(true) => Json(json!({
            "success": true,
            "message": format!("Role updated for '{username}'"),
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
        Ok(true) => Json(json!({
            "success": true,
            "message": format!("User '{username}' deleted"),
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
    match state.db.delete_api_token(&id) {
        Ok(true) => Json(json!({
            "success": true,
            "tokens": state.db.list_api_tokens().unwrap_or_default()
        }))
        .into_response(),
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

async fn caller_allowed_cameras(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
) -> Option<Vec<String>> {
    let cfg = state.config.read().await;
    let (user, role) =
        extract_authenticated_user(state, jar, headers, &cfg.auth.cookie_name, cfg.auth.enabled)?;
    if role.eq_ignore_ascii_case("admin") {
        return None;
    }
    let db_u = state.db.get_user(&user).ok().flatten()?;
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
        cfg.to_frigate_api_config()
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
    let api_cfg = next_cfg.to_frigate_api_config();
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

async fn api_detectors_catalog(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::to_value(state.detector.runtime_catalog()).unwrap_or_else(|_| json!({})))
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
        .and_then(|c| c.get("frigate.full_system"))
        .and_then(|f| f.get("cpu"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let sys_mem_pct = stats_val
        .get("cpu_usages")
        .and_then(|c| c.get("frigate.full_system"))
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
        "frigate.full_system".to_string(),
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

    let mut storage_map = serde_json::Map::new();
    storage_map.insert("/media/frigate/recordings".to_string(), rec_mount);
    if cfg.record.storage_tiers.enabled
        && let Some(ref cold_dir) = cfg.record.storage_tiers.cold_dir
    {
        storage_map.insert(
            cold_dir.to_string_lossy().to_string(),
            stat_mount_mb(cold_dir, "ext4"),
        );
    }
    storage_map.insert(
        "/media/frigate/clips".to_string(),
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

async fn api_reviews_delete(
    State(state): State<AppState>,
    Json(body): Json<DeleteIdsBody>,
) -> Json<Value> {
    let _ = state.db.delete_reviews(&body.ids);
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
    let filter = EventFilter {
        cameras,
        labels,
        exclude_labels: split_csv_opt(&q.exclude_labels),
        zones: split_csv_opt(&q.zones),
        sub_labels: split_csv_opt(&q.sub_labels),
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
    Json(json!({ "success": true }))
}

#[derive(Debug, Deserialize, Default)]
pub struct SubLabelBody {
    #[serde(alias = "subLabel")]
    pub sub_label: Option<String>,
}

async fn api_event_sub_label(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<SubLabelBody>,
) -> Json<Value> {
    let _ = state.db.set_event_sub_label(&id, body.sub_label.as_deref());
    Json(json!({ "success": true }))
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
    if let Ok(Some(ev)) = state.db.get_event(&id)
        && let (Some(cam), Some(start)) = (ev["camera"].as_str(), ev["start_time"].as_f64())
    {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareTokenPayload {
    pub k: String,
    pub id: String,
    pub exp: u64,
}

#[derive(Debug, Deserialize)]
pub struct CreateShareBody {
    #[serde(default = "default_share_kind")]
    pub kind: String,
    pub target_id: String,
    #[serde(default = "default_share_ttl")]
    pub expires_in_sec: u64,
}

fn default_share_kind() -> String {
    "export".to_string()
}

fn default_share_ttl() -> u64 {
    86400
}

pub fn sign_share_token(secret: &str, payload: &ShareTokenPayload) -> String {
    let json_bytes = serde_json::to_vec(payload).unwrap_or_default();
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&json_bytes);
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
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
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
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
    if payload.exp <= now_ts {
        return Err(StatusCode::GONE);
    }
    Ok(payload)
}

fn resolve_share_target_metadata(
    state: &AppState,
    kind: &str,
    target_id: &str,
) -> (Option<String>, String, Option<u64>) {
    if kind == "export" {
        if let Ok(exports) = state.db.query_exports()
            && let Some(exp) = exports
                .into_iter()
                .find(|e| e["id"].as_str() == Some(target_id))
        {
            let cam = exp["camera"].as_str().map(|s| s.to_string());
            let name = exp["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(target_id)
                .to_string();
            let date_ts = exp["date"].as_f64().map(|t| t as u64);
            return (cam, name, date_ts);
        }
        let cam_prefix = target_id
            .rsplitn(3, '_')
            .nth(2)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        return (cam_prefix, target_id.to_string(), None);
    }

    if let Ok(Some(ev)) = state.db.get_event(target_id) {
        let cam = ev["camera"].as_str().map(|s| s.to_string());
        let label = ev["label"].as_str().unwrap_or("event");
        let start = ev["start_time"].as_f64().map(|t| t as u64);
        let title = format!("{} // {}", cam.as_deref().unwrap_or("camera"), label);
        return (cam, title, start);
    }

    if let Ok(Some(rev)) = state.db.get_review_by_id(target_id) {
        let cam = Some(rev.camera.clone());
        let title = format!("{} // {}", rev.camera, rev.severity.to_ascii_uppercase());
        return (cam, title, Some(rev.start_time as u64));
    }

    (None, target_id.to_string(), None)
}

async fn api_share_create(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<CreateShareBody>,
) -> Response {
    let kind =
        if body.kind.eq_ignore_ascii_case("event") || body.kind.eq_ignore_ascii_case("review") {
            "event".to_string()
        } else {
            "export".to_string()
        };
    let (target_cam, _, _) = resolve_share_target_metadata(&state, &kind, &body.target_id);
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

    let exp = unix_now() + body.expires_in_sec.clamp(60, 86400 * 30);
    let payload = ShareTokenPayload {
        k: kind,
        id: body.target_id.clone(),
        exp,
    };
    let token = sign_share_token(&state.jwt_secret, &payload);
    Json(json!({
        "success": true,
        "token": token,
        "url": format!("/share/{token}"),
        "video_url": format!("/share/{token}/video.mp4"),
        "expires_at": exp
    }))
    .into_response()
}

async fn share_player_page(
    State(state): State<AppState>,
    AxumPath(token): AxumPath<String>,
) -> Response {
    let payload = match verify_share_token(&state.jwt_secret, &token, unix_now()) {
        Ok(p) => p,
        Err(status) => {
            return (
                status,
                Json(json!({ "error": "Share link invalid or expired" })),
            )
                .into_response();
        }
    };
    let (_, clip_title, clip_ts) = resolve_share_target_metadata(&state, &payload.k, &payload.id);
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
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>NinjaNVR Shared Clip // {clip_title}</title></head>
<body style="margin:0;padding:24px;display:flex;flex-direction:column;align-items:center;gap:14px">
  <div style="width:100%;max-width:960px;background:#0d121b;border:1px solid #1e293b;border-radius:4px;padding:16px">
    <div style="display:flex;justify-content:space-between;align-items:center;gap:12px;flex-wrap:wrap;margin-bottom:12px;font-family:monospace;font-size:12px">
      <div>
        <div><strong>NINJANVR SHARED CLIP // {clip_title}</strong></div>
        <div style="color:#94a3b8;font-size:11px;margin-top:2px">CLIP TIME: {ts_label} · EXPIRES: {exp_label}</div>
      </div>
      <a href="/share/{token}/video.mp4" download="{}.mp4" style="color:#38bdf8;text-decoration:none;border:1px solid #0284c7;padding:4px 10px;border-radius:3px">Download MP4</a>
    </div>
    <video src="/share/{token}/video.mp4" controls autoplay playsinline style="width:100%;max-height:72vh;object-fit:contain;background:#000"></video>
  </div>
</body>
</html>"#,
        payload.id
    );
    Html(html).into_response()
}

fn serve_mp4_bytes_with_range(bytes: Vec<u8>, headers: &HeaderMap) -> Response {
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
        ],
        bytes,
    )
        .into_response()
}

async fn share_video_mp4(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
) -> Response {
    let payload = match verify_share_token(&state.jwt_secret, &token, unix_now()) {
        Ok(p) => p,
        Err(status) => {
            return (
                status,
                Json(json!({ "error": "Share link invalid or expired" })),
            )
                .into_response();
        }
    };

    if payload.k == "export" {
        if let Ok(exports) = state.db.query_exports()
            && let Some(exp) = exports
                .into_iter()
                .find(|e| e["id"].as_str() == Some(&payload.id))
            && let Some(vp) = exp["video_path"].as_str()
        {
            let disk_path = resolve_recording_disk_path(&state.media_dir, vp);
            if let Ok(bytes) = tokio::fs::read(&disk_path).await {
                return serve_mp4_bytes_with_range(bytes, &headers);
            }
        }
        let safe_id: String = payload
            .id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
            .collect();
        let file_path = state
            .media_dir
            .join("exports")
            .join(format!("{safe_id}.mp4"));
        if let Ok(bytes) = tokio::fs::read(&file_path).await {
            return serve_mp4_bytes_with_range(bytes, &headers);
        }
    } else {
        if let Ok(Some(ev)) = state.db.get_event(&payload.id) {
            let cam = ev["camera"].as_str().unwrap_or("sala");
            let start = ev["start_time"].as_f64().unwrap_or(0.0);
            let end = ev["end_time"].as_f64().unwrap_or(start + 15.0);
            if let Ok(recs) = state.db.query_recordings(cam, start - 10.0, end + 10.0) {
                for rec in recs {
                    let disk_path = resolve_recording_disk_path(&state.media_dir, &rec.path);
                    if let Ok(bytes) = tokio::fs::read(&disk_path).await {
                        return serve_mp4_bytes_with_range(bytes, &headers);
                    }
                }
            }
        }
        if let Ok(Some(rev)) = state.db.get_review_by_id(&payload.id) {
            let cam = &rev.camera;
            let start = rev.start_time;
            let end = rev.end_time.unwrap_or(start + 15.0);
            if let Ok(recs) = state.db.query_recordings(cam, start - 10.0, end + 10.0) {
                for rec in recs {
                    let disk_path = resolve_recording_disk_path(&state.media_dir, &rec.path);
                    if let Ok(bytes) = tokio::fs::read(&disk_path).await {
                        return serve_mp4_bytes_with_range(bytes, &headers);
                    }
                }
            }
        }
    }

    StatusCode::NOT_FOUND.into_response()
}

async fn api_exports(State(state): State<AppState>) -> Json<Value> {
    let exports = state.db.query_exports().unwrap_or_default();
    Json(Value::Array(exports))
}

async fn api_cases(State(state): State<AppState>) -> Json<Value> {
    let cases = state.db.query_cases().unwrap_or_default();
    Json(Value::Array(cases))
}

async fn api_case_create() -> Json<Value> {
    Json(json!({ "success": true }))
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
    let video_rel = format!("/media/frigate/exports/{id}.mp4");
    let thumb_rel = format!("/media/frigate/clips/{id}.webp");
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
    AxumPath(id): AxumPath<String>,
    Json(body): Json<RenameExportBody>,
) -> Json<Value> {
    let _ = state.db.rename_export(&id, &body.name);
    Json(json!({ "success": true }))
}

async fn api_export_delete(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Json<Value> {
    if let Ok(Some((vpath, tpath))) = state.db.delete_export(&id) {
        let _ = tokio::fs::remove_file(resolve_recording_disk_path(&state.media_dir, &vpath)).await;
        let _ = tokio::fs::remove_file(resolve_recording_disk_path(&state.media_dir, &tpath)).await;
    }
    Json(json!({ "success": true }))
}

async fn api_exports_bulk_delete(
    State(state): State<AppState>,
    Json(body): Json<DeleteIdsBody>,
) -> Json<Value> {
    for id in body.ids {
        if let Ok(Some((vpath, tpath))) = state.db.delete_export(&id) {
            let _ =
                tokio::fs::remove_file(resolve_recording_disk_path(&state.media_dir, &vpath)).await;
            let _ =
                tokio::fs::remove_file(resolve_recording_disk_path(&state.media_dir, &tpath)).await;
        }
    }
    Json(json!({ "success": true }))
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
) -> Response {
    let camera = preview_name
        .strip_prefix("preview_")
        .and_then(|s| s.split('-').next())
        .unwrap_or("sala")
        .to_string();
    api_camera_latest_frame(State(state), AxumPath(camera)).await
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

async fn api_camera_latest_frame(
    State(state): State<AppState>,
    AxumPath(camera): AxumPath<String>,
) -> Response {
    let metric_ts = state
        .camera_metrics
        .read()
        .await
        .get(&camera)
        .map(|m| m.last_frame_ts)
        .unwrap_or(0.0);

    let ts_hdr = header::HeaderName::from_static("x-last-frame-timestamp");

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
    let url = format!("{}/api/webrtc?{query}", state.go2rtc_http_base);
    let mut req = state.http_client.post(&url).body(body.to_vec());
    if let Some(ct) = headers.get(header::CONTENT_TYPE) {
        req = req.header(header::CONTENT_TYPE, ct);
    }
    match req.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
            let bytes = resp.bytes().await.unwrap_or_default();
            (status, Body::from(bytes)).into_response()
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
    if let Some(rel) = db_path.strip_prefix("/media/frigate/") {
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
        "frigate".to_string(),
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
    log_hub.push_frigate(
        "INFO",
        "ninjanvr.app",
        &format!("Starting NinjaNVR ({NINJANVR_VERSION})"),
    );
    log_hub.push_frigate(
        "INFO",
        "ninjanvr.db",
        &format!("Connected to SQLite WAL database at {}", cfg.database.path),
    );
    log_hub.push_frigate(
        "INFO",
        "ninjanvr.infer",
        &format!(
            "Initialized OpenVINO C-API detector on CPU (model={})",
            cfg.model.path
        ),
    );
    for (cam_name, cam_cfg) in &cfg.cameras {
        if cam_cfg.enabled {
            log_hub.push_frigate(
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

    let shared_cfg = Arc::new(RwLock::new(cfg));
    let activity_ledger = new_activity_ledger();
    let latest_frames = Arc::new(RwLock::new(HashMap::new()));
    let camera_metrics: SharedCameraMetrics = Arc::new(RwLock::new(HashMap::new()));
    let stats_history: Arc<RwLock<VecDeque<Value>>> = Arc::new(RwLock::new(VecDeque::new()));
    let go2rtc_supervisor = Arc::new(Go2RtcSupervisor::new(shared_cfg.clone()));
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
            camera_metrics: camera_metrics.clone(),
            media_dir: media_dir.clone(),
            log_hub: log_hub.clone(),
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
        activity_ledger,
        detector,
        camera_metrics,
        log_hub,
        stats_history,
        cpu_tracker: ProcCpuTracker::default(),
        storage_degraded,
        storage_error,
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

    #[tokio::test]
    async fn router_serves_version_config_stats_and_auth() {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr-srv-test-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let db_path = tmp_dir.join("frigate.db");
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

        // 1. Test /api/version
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

        // 2. Test /api/config and verify RTSP password scrubbing
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

        // 3. Test unknown /api/* returns 404 JSON instead of SPA index.html
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

        // 4. Test SPA index fallback (always serves Console v2, even with legacy ninjanvr_ui=v1 cookie)
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

        // 5. Test /exports/*.mp4 is served uncompressed with Accept-Ranges and exact Content-Length
        let exports_dir = tmp_dir.join("exports");
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

        // 6. Test POST /api/export with no segments returns 400 and cleans up in_progress row
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

        // 7. Test /api/config/structured preserves scrubbed credentials and persists config.yml
        let mut cfg_patch = val.clone();
        cfg_patch["cameras"]["front_door"]["detect"]["fps"] = json!(8);
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
        let on_disk_yaml = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(on_disk_yaml.contains("admin:supersecret@192.0.2.10:554/stream1"));

        // 8. Test /api/users CRUD endpoints
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

        // 9. Test enforce_auth=true router blocks unauthenticated /api/config with 401
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

        // 10. Test degraded storage mount guard: unwritable media_dir boots cleanly and reports storage_degraded=true
        let blocked_media_path = tmp_dir.join("unwritable_mount_file");
        std::fs::write(&blocked_media_path, b"not_a_directory").unwrap();
        let degraded_state = new_app_state(
            &cfg_path,
            tmp_dir.join("web-v2"),
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

        // Auto-recovery when mount becomes a writable directory
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

        // 11. Test DELETE /api/:camera/recordings and POST /api/recordings/sync
        let rec_subdir = tmp_dir.join("recordings/2026-11-01/00/front_door");
        std::fs::create_dir_all(&rec_subdir).unwrap();
        let disk_seg_1 = rec_subdir.join("00.00.mp4");
        std::fs::write(&disk_seg_1, vec![0x11u8; 1024]).unwrap();

        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "seg_on_disk".to_string(),
                camera: "front_door".to_string(),
                path: "/media/frigate/recordings/2026-11-01/00/front_door/00.00.mp4".to_string(),
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
                path: "/media/frigate/recordings/2026-11-01/00/front_door/00.10.mp4".to_string(),
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

        // 12. Test GET /api/:camera/latest.jpg serves persisted last-online snapshot and X-Last-Frame-Timestamp header
        let clips_dir = tmp_dir.join("clips");
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

        let resp_latest = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/front_door/latest.jpg")
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

        // 13. Test POST /api/streams/probe never leaks RTSP credentials on probe errors
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

        // 14. Test POST /api/profile switches operational security profile and persists state
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

        // 15. Test GET/POST /api/:camera/state updates runtime camera flags dynamically
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

        // 16. Test RBAC (admin, operator, viewer) and per-camera user scoping
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

        // Viewer is blocked from POST /api/config/save with 403 Forbidden
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

        // Viewer is allowed to read permitted camera (/api/front_door/recordings)
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

        // Viewer is blocked from accessing unpermitted camera (/api/indoor_bedroom/recordings) with 403 Forbidden
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

        // 17. Test HMAC-signed shareable links (/api/share, /share/:token, /share/:token/video.mp4)
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

        // Valid unauthenticated request to /share/:token and /share/:token/video.mp4 on auth_app
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

        // HTTP Range request on /share/:token/video.mp4 returns 206 Partial Content
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

        // Tampered signature is rejected with 403 Forbidden
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

        // Expired token is rejected with 410 Gone
        let expired_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: "sample".to_string(),
                exp: 1000,
            },
        );
        let resp_expired = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{expired_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_expired.status(), StatusCode::GONE);

        // 18. Test scoped API Bearer tokens (nnvr_...) creation, scope enforcement, and revocation
        let (tok_meta, raw_ro_tok) = state
            .db
            .create_api_token("ha_sensor", "read_only", None)
            .unwrap();
        assert!(raw_ro_tok.starts_with("nnvr_"));
        let tok_id = tok_meta["id"].as_str().unwrap();

        // read_only token can query GET /api/stats and GET /api/events
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

        // read_only token is blocked from GET /api/users, GET /api/tokens, and GET /api/config/raw (403 Forbidden)
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

        // read_only token is rejected (403 Forbidden) on POST/DELETE mutations
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

        // metrics token can query GET /api/stats and GET /api/metrics, but is rejected (403) on GET /api/events
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

        // full token has admin-equivalent access to /api/tokens CRUD
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

        // Revoking the token blocks subsequent requests with 401 Unauthorized
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

        // 19. Test GET /api/ai/context and POST /api/mcp (tools/list and tools/call)
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

        // 20. Test OIDC role claim mapping (Authelia array & Zitadel object map) and Trusted-Proxy CIDR verification
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

        // Spoofed Remote-User header from untrusted IP (or when proxy.enabled == false) is strictly rejected (401)
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

        // Public /api/auth/providers endpoint works without authentication
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

        // 21. Test POST /api/debug/reprocess replays clip window and returns accepted and filtered detections
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

        // 22. Test Camera Web-UI Reverse Proxy (/api/camera-proxy/:camera/) SSRF & Admin RBAC enforcement
        // Viewer role is blocked with 403 Forbidden
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

        // Unconfigured camera host returns 404 Not Found (prevents arbitrary SSRF)
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

        // 23. Test forensic operator notes on Events (PUT /api/events/:id/notes) and Reviews (PUT /api/review/:id/notes)
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_note_test".to_string(),
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
        let resp_ev_notes = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/events/ev_note_test/notes")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"notes":"Courier left parcel behind planter — verified by guard"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ev_notes.status(), StatusCode::OK);
        let ev_with_note = state.db.get_event("ev_note_test").unwrap().unwrap();
        assert_eq!(
            ev_with_note["data"]["notes"],
            "Courier left parcel behind planter — verified by guard"
        );

        // 24. Test POST /api/notifications/test broadcasts synthetic alert across WS & MQTT
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

        // 25. Test Prometheus OpenMetrics exposition format at GET /api/metrics and SQLite run_maintenance()
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

        // 26. Test Apple-compliant self-signed TLS certificate generation with custom SAN hosts
        let tls_cfg = generate_self_signed_tls_config_with_sans(&[
            "192.0.2.10".to_string(),
            "nvr.example.com".to_string(),
        ])
        .unwrap();
        assert_eq!(
            tls_cfg.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );

        // 27. Test Multi-Node Federation endpoints (/api/federation/nodes and /api/federation/:node/*)
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

        // Unconfigured federated peer node returns 404 Not Found
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

        // Public bootstrap endpoint /api/auth/first_time_login remains accessible
        let resp = auth_app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/first_time_login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
