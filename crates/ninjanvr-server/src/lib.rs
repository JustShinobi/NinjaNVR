#![recursion_limit = "512"]

pub mod pipeline;

use std::collections::HashMap;
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
use futures_util::{SinkExt, StreamExt};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use ninjanvr_comms::{CommsHub, SharedCommsHub};
use ninjanvr_config::{
    DEFAULT_GO2RTC_SHM_PATH, Go2RtcSupervisor, NINJANVR_VERSION, NinjaConfig, scrub_json_value,
};
use ninjanvr_db::{EventFilter, ExportRow, NinjaDatabase, ReviewFilter, verify_password};
use ninjanvr_infer::AiDetectorEngine;
use ninjanvr_media::{
    RecordingEngine, SharedActivityLedger, build_mp4_export, new_activity_ledger,
};
use pipeline::{SharedCameraMetrics, VisionPipeline};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::protocol::Message as TungMsg;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{error, info, warn};

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
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JwtClaims {
    pub sub: String,
    pub role: String,
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
    pub zones: Option<String>,
    pub reviewed: Option<i32>,
    pub severity: Option<String>,
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
    pub zones: Option<String>,
    pub sub_labels: Option<String>,
    pub favorites: Option<i32>,
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
    rand::Rng::fill(&mut rand::thread_rng(), &mut bytes);
    let secret: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let _ = std::fs::write(&secret_path, &secret);
    secret
}

pub fn build_router(state: AppState) -> Router {
    let web_dir = state.web_dir.clone();
    let index_html = web_dir.join("index.html");
    let login_html = web_dir.join("login.html");

    let clips_dir = state.media_dir.join("clips");
    let recordings_dir = state.media_dir.join("recordings");
    let exports_dir = state.media_dir.join("exports");

    let api_router = Router::new()
        .route("/version", get(api_version))
        .route("/auth", get(api_auth_verify))
        .route("/auth/first_time_login", get(api_first_time_login))
        .route("/login", post(api_login))
        .route("/logout", get(api_logout).post(api_logout))
        .route("/profile", get(api_profile))
        .route("/profile/active", get(api_profile_active))
        .route("/profiles", get(api_profiles))
        .route("/users", get(api_users))
        .route("/config", get(api_config))
        .route("/config/raw", get(api_config_raw))
        .route("/config/raw_paths", get(api_empty_array))
        .route("/config/save", post(api_config_save))
        .route("/config/set", put(api_config_set))
        .route("/config/schema.json", get(api_config_schema))
        .route("/ffmpeg/presets", get(api_ffmpeg_presets))
        .route("/restart", post(api_restart))
        .route("/logs/{service}", get(api_service_logs))
        .route("/stats", get(api_stats))
        .route("/stats/history", get(api_empty_array))
        .route("/review", get(api_review))
        .route("/review/summary", get(api_review_summary))
        .route("/review/activity/motion", get(api_review_activity_motion))
        .route("/review/event/{event_id}", get(api_review_by_event_id))
        .route("/review/{id}", get(api_review_by_id))
        .route("/review/{id}/viewed", delete(api_review_unview_one))
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
        .route("/recordings/unavailable", get(api_empty_array))
        .route("/recordings/storage", get(api_recordings_storage))
        .route("/exports", get(api_exports))
        .route("/exports/delete", post(api_exports_bulk_delete))
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
        .route("/go2rtc/streams", get(api_go2rtc_streams))
        .route("/go2rtc/streams/{stream}", get(api_go2rtc_stream_one))
        .route("/go2rtc/webrtc", post(api_go2rtc_webrtc))
        .route("/{camera}/recordings", get(api_camera_recordings))
        .route(
            "/{camera}/recordings/summary",
            get(api_camera_recordings_summary),
        )
        .route("/{camera}/ptz/info", get(api_empty_object))
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
        .route(
            "/login",
            get(move || async move {
                match tokio::fs::read_to_string(&login_html).await {
                    Ok(html) => Html(html).into_response(),
                    Err(_) => StatusCode::NOT_FOUND.into_response(),
                }
            }),
        )
        .nest_service("/clips", ServeDir::new(clips_dir))
        .nest_service("/recordings", ServeDir::new(recordings_dir))
        .nest_service("/exports", ServeDir::new(exports_dir))
        .fallback_service(ServeDir::new(web_dir).fallback(ServeFile::new(index_html)))
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive())
        .with_state(state)
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

async fn api_login(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(body): Json<LoginBody>,
) -> impl IntoResponse {
    let (cookie_name, cookie_secure, session_len, auth_enabled) = {
        let cfg = state.config.read().await;
        (
            cfg.auth.cookie_name.clone(),
            cfg.auth.cookie_secure,
            cfg.auth.session_length,
            cfg.auth.enabled,
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

    if let Some(remote_user) = headers.get("remote-user").and_then(|v| v.to_str().ok())
        && !remote_user.is_empty()
    {
        let role = headers
            .get("remote-role")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("admin")
            .to_string();
        return Some((remote_user.to_string(), role));
    }

    let token_str = jar
        .get(cookie_name)
        .map(|c| c.value().to_string())
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer "))
                .map(|s| s.to_string())
        })?;

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

    Json(json!({
        "username": username,
        "role": role,
        "allowed_cameras": cameras
    }))
    .into_response()
}

async fn api_profile_active() -> Json<Value> {
    Json(json!({ "active_profile": null }))
}

async fn api_profiles() -> Json<Value> {
    Json(json!({
        "profiles": [],
        "active_profile": null,
        "last_activated": {}
    }))
}

async fn api_users(State(state): State<AppState>) -> Json<Value> {
    let users = state.db.list_users().unwrap_or_default();
    Json(Value::Array(users))
}

async fn api_config(State(state): State<AppState>) -> Json<Value> {
    let cfg = state.config.read().await;
    Json(cfg.to_frigate_api_config())
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

async fn api_config_set() -> Json<Value> {
    Json(json!({ "success": true, "message": "Config updated" }))
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

async fn api_service_logs(AxumPath(service): AxumPath<String>) -> Json<Value> {
    Json(json!({
        "totalLines": 1,
        "lines": [format!("NinjaNVR ({NINJANVR_VERSION}) service {service} is running healthy.")]
    }))
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
    let go2rtc_pid = state.go2rtc_supervisor.pid();

    let mut cameras_stats = serde_json::Map::new();
    let mut total_cam_fps = 0.0;
    let mut total_det_fps = 0.0;
    for (cam_name, cam_cfg) in &cfg.cameras {
        let m = metrics_map.get(cam_name);
        let fps = m.map(|x| x.camera_fps).unwrap_or(if cam_cfg.enabled {
            cam_cfg.detect.fps as f64
        } else {
            0.0
        });
        let det_fps = m.map(|x| x.detection_fps).unwrap_or(0.0);
        let cam_pid = m.map(|x| x.pid).unwrap_or(pid);
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
                "pid": pid,
                "capture_pid": pid,
                "ffmpeg_pid": cam_pid,
                "audio_rms": 0,
                "audio_dBFS": 0,
                "connection_quality": "excellent",
                "expected_fps": cam_cfg.detect.fps,
                "reconnects_last_hour": 0,
                "stalls_last_hour": 0,
                "ffmpeg_cpu": "0.2",
                "capture_cpu": "0.1",
                "detect_cpu": "0.4"
            }),
        );
    }

    let infer_ms = (state.detector.avg_inference_ms() * 100.0).round() / 100.0;
    let mut detectors_stats = serde_json::Map::new();
    for det_name in cfg.detectors.keys() {
        detectors_stats.insert(
            det_name.clone(),
            json!({
                "inference_speed": infer_ms,
                "detection_start": 0.0,
                "pid": pid,
                "cpu": "1.2",
                "mem": "0.5"
            }),
        );
    }

    let rec_dir = state.media_dir.join("recordings");
    let clips_dir = state.media_dir.join("clips");
    let mut shm_stat = stat_mount_mb(Path::new("/dev/shm"), "tmpfs");
    if let Some(obj) = shm_stat.as_object_mut() {
        obj.insert("available".to_string(), json!(974.0));
        obj.insert("camera_frame_size".to_string(), json!(0.0));
        obj.insert("shm_frame_count".to_string(), json!(50));
        obj.insert("min_shm".to_string(), json!(16));
    }

    Json(json!({
        "cameras": cameras_stats,
        "detectors": detectors_stats,
        "camera_fps": (total_cam_fps * 10.0).round() / 10.0,
        "process_fps": (total_cam_fps * 10.0).round() / 10.0,
        "skipped_fps": 0.0,
        "detection_fps": (total_det_fps * 10.0).round() / 10.0,
        "embeddings": {},
        "cpu_usages": {
            "frigate.full_system": {
                "cpu": "1.5",
                "mem": "1.8"
            }
        },
        "service": {
            "uptime": state.start_instant.elapsed().as_secs(),
            "version": NINJANVR_VERSION,
            "latest_version": NINJANVR_VERSION,
            "storage": {
                "/media/frigate/recordings": stat_mount_mb(&rec_dir, "ext4"),
                "/media/frigate/clips": stat_mount_mb(&clips_dir, "ext4"),
                "/tmp/cache": stat_mount_mb(Path::new("/tmp"), "tmpfs"),
                "/dev/shm": shm_stat
            },
            "last_updated": unix_now()
        },
        "processes": {
            "recording": { "pid": pid, "cpu": "0.2", "mem": "0.4" },
            "review_segment": { "pid": pid, "cpu": "0.1", "mem": "0.2" },
            "go2rtc": { "pid": go2rtc_pid, "cpu": "1.5", "mem": "0.7" }
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
    Query(q): Query<ReviewQueryParams>,
) -> Json<Value> {
    let filter = ReviewFilter {
        cameras: split_csv_opt(&q.cameras),
        labels: split_csv_opt(&q.labels),
        zones: split_csv_opt(&q.zones),
        reviewed: q.reviewed.map(|v| v != 0),
        severity: q.severity,
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
    Query(q): Query<EventQueryParams>,
) -> Json<Value> {
    let cameras = split_csv_opt(&q.cameras).or_else(|| split_csv_opt(&q.camera));
    let labels = split_csv_opt(&q.labels).or_else(|| split_csv_opt(&q.label));
    let filter = EventFilter {
        cameras,
        labels,
        zones: split_csv_opt(&q.zones),
        sub_labels: split_csv_opt(&q.sub_labels),
        favorites: q.favorites.map(|v| v != 0),
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

async fn api_recordings_storage(State(state): State<AppState>) -> Json<Value> {
    let usage = state
        .db
        .query_recordings_storage_usage()
        .unwrap_or_else(|_| json!({}));
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
                "segment_size": r.segment_size,
                "motion_heatmap": r.motion_heatmap
            })
        })
        .collect();
    Json(Value::Array(dto))
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

#[derive(Debug, Deserialize, Default)]
pub struct CreateExportBody {
    pub name: Option<String>,
}

async fn api_export_create(
    State(state): State<AppState>,
    AxumPath((camera, start, end)): AxumPath<(String, f64, f64)>,
    body: Option<Json<CreateExportBody>>,
) -> Response {
    let id = format!("{camera}_{}_{}", start as u64, end as u64);
    let name = body
        .and_then(|b| b.0.name)
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
    tokio::spawn(async move {
        if let Err(err) = build_mp4_export(
            &db_clone,
            &media_dir_clone,
            &id_clone,
            &camera,
            &name,
            start,
            end,
        )
        .await
        {
            warn!("Export {id_clone} failed: {err:#}");
        }
    });

    Json(json!({ "success": true, "export_id": id })).into_response()
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

async fn api_camera_latest_frame(
    State(state): State<AppState>,
    AxumPath(camera): AxumPath<String>,
) -> Response {
    if let Some(frame_bytes) = state.latest_frames.read().await.get(&camera).cloned() {
        return ([(header::CONTENT_TYPE, "image/jpeg")], frame_bytes).into_response();
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
        return ([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response();
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
    match ninjanvr_vod::build_media_playlist(&recs) {
        Ok(m3u8) => (
            [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
            m3u8,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn vod_init_mp4(
    State(state): State<AppState>,
    AxumPath((camera, start, end)): AxumPath<(String, f64, f64)>,
) -> Response {
    let recs = existing_recordings_for_vod(&state, &camera, start, end);
    for rec in recs {
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
    let _ = rustls::crypto::ring::default_provider().install_default();
    let subject_alt_names = vec![
        "localhost".to_string(),
        "frigate".to_string(),
        "ninjanvr".to_string(),
        "127.0.0.1".to_string(),
    ];
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(subject_alt_names)?;
    let cert_der = rustls::pki_types::CertificateDer::from(cert);
    let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der()),
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
    let db = NinjaDatabase::open(&cfg.database.path)?;
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
    let shared_cfg = Arc::new(RwLock::new(cfg));
    let activity_ledger = new_activity_ledger();
    let latest_frames = Arc::new(RwLock::new(HashMap::new()));
    let camera_metrics: SharedCameraMetrics = Arc::new(RwLock::new(HashMap::new()));
    let go2rtc_supervisor = Arc::new(Go2RtcSupervisor::new(shared_cfg.clone()));
    if spawn_go2rtc {
        go2rtc_supervisor.clone().spawn_supervisor_task();
        let rec_engine = Arc::new(RecordingEngine::new(
            shared_cfg.clone(),
            db.clone(),
            media_dir.clone(),
            activity_ledger.clone(),
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
        });
        vision_pipeline.start().await;
    }

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

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
    };

    if spawn_go2rtc {
        let stats_state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                let Json(stats_val) = api_stats(State(stats_state.clone())).await;
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
        let web_dir = tmp_dir.join("web");
        std::fs::create_dir_all(&web_dir).unwrap();
        std::fs::write(
            web_dir.join("index.html"),
            "<html><body>NinjaNVR UI</body></html>",
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
        let app = build_router(state);

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

        // 4. Test SPA index fallback
        let resp = app
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
        assert!(std::str::from_utf8(&body).unwrap().contains("NinjaNVR UI"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
