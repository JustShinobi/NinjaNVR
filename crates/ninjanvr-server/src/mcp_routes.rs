#![forbid(unsafe_code)]
//! AI Security Digest (`/api/ai/context`) and MCP Streamable HTTP JSON-RPC 2.0 (`/api/mcp`) routes.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum_extra::extract::cookie::CookieJar;
use ninjanvr_config::NINJANVR_VERSION;
use ninjanvr_db::EventFilter;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth_gate::{
    authenticate_read, caller_allowed_cameras, extract_authenticated_user_with_cfg,
    request_requires_strict_auth, scope_cameras_filter, verify_cookie_csrf_origin,
    warm_auth_cache_for_request,
};
use crate::telemetry::is_camera_stream_online;
use crate::{AppState, redact_event_slice_thumbnails_if_masked, unix_now};

#[derive(Debug, Deserialize, Default)]
pub struct AiContextQueryParams {
    pub hours: Option<f64>,
    pub after: Option<f64>,
    pub before: Option<f64>,
}

pub(crate) async fn build_ai_context_digest(
    state: &AppState,
    hours: f64,
    after_opt: Option<f64>,
    before_opt: Option<f64>,
    allowed_cameras: Option<&[String]>,
) -> Value {
    let now = unix_now() as f64;
    let before = before_opt.filter(|v| v.is_finite()).unwrap_or(now + 5.0);
    let safe_hours = if hours.is_finite() { hours } else { 24.0 };
    let clamped_hours = safe_hours.clamp(0.1, 720.0);
    let after = after_opt
        .filter(|v| v.is_finite())
        .unwrap_or(before - clamped_hours * 3600.0);

    let cfg = state.config.read().await;
    let metrics_map = state.camera_metrics.read().await;
    let mut cameras_health = serde_json::Map::new();
    for cam_name in cfg.cameras.keys() {
        if let Some(allowed) = allowed_cameras
            && !allowed.iter().any(|a| a == cam_name || a == "*")
        {
            continue;
        }
        let m = metrics_map.get(cam_name);
        let rt_flags = state.comms.get_camera_flags(cam_name);
        let fps = m.map_or(0.0, |x| x.camera_fps);
        cameras_health.insert(
            cam_name.clone(),
            json!({
                "enabled": rt_flags.enabled,
                "online": is_camera_stream_online(fps),
                "fps": fps,
                "detection_fps": m.map_or(0.0, |x| x.detection_fps),
                "night_mode": m.is_some_and(|x| x.night_mode),
                "detect": rt_flags.detect,
                "record": rt_flags.record,
                "alerts": rt_flags.alerts
            }),
        );
    }
    drop(metrics_map);
    drop(cfg);

    let db_allowed_owned: Option<Vec<String>> = match allowed_cameras {
        Some(list) if list.iter().any(|a| a == "*") => None,
        Some(list) => Some(list.to_vec()),
        None => None,
    };
    let db_clone = state.db.clone();
    let db_digest = tokio::task::spawn_blocking(move || {
        db_clone
            .query_ai_digest_summary(after, before, db_allowed_owned.as_deref())
            .unwrap_or_else(|_| json!({}))
    })
    .await
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

pub async fn api_ai_context(
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

/// Handles `GET /api/mcp` for MCP Streamable HTTP clients:
/// returns a keep-alive `text/event-stream` when `Accept` includes `text/event-stream`,
/// or a `200 OK` JSON endpoint descriptor otherwise.
pub async fn api_mcp_get_handler(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Response {
    let principal = match authenticate_read(&state, &jar, &headers).await {
        Ok(p) => p,
        Err(rej) => return rej.into_response(),
    };
    if principal.role.trim().eq_ignore_ascii_case("metrics") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "message": "Token scope 'metrics' is not permitted to access MCP"
            })),
        )
            .into_response();
    }
    let accepts_sse = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.to_ascii_lowercase().contains("text/event-stream"));
    if accepts_sse {
        let stream = futures_util::stream::empty::<Result<SseEvent, Infallible>>();
        Sse::new(stream)
            .keep_alive(
                KeepAlive::new()
                    .interval(Duration::from_secs(15))
                    .text("keepalive"),
            )
            .into_response()
    } else {
        Json(json!({
            "name": "ninjanvr-mcp",
            "version": NINJANVR_VERSION,
            "protocolVersion": "2024-11-05",
            "status": "ok"
        }))
        .into_response()
    }
}

pub async fn api_mcp_handler(
    State(state): State<AppState>,
    uri: Uri,
    jar: CookieJar,
    headers: HeaderMap,
    Json(req): Json<McpJsonRpcRequest>,
) -> Response {
    let _ = &req.jsonrpc;
    let id = req.id.clone();
    if !verify_cookie_csrf_origin(&headers, &uri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32003,
                    "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
                }
            })),
        )
            .into_response();
    }
    let caller_role = {
        let auth_cfg = state.config.read().await.auth.clone();
        warm_auth_cache_for_request(&state, &jar, &headers, &auth_cfg).await;
        let must_auth = request_requires_strict_auth(&headers, &jar, &auth_cfg);
        match extract_authenticated_user_with_cfg(&state, &jar, &headers, &auth_cfg) {
            Some((_, r)) => r.to_ascii_lowercase(),
            None if auth_cfg.enabled && must_auth => "unauthorized".to_string(),
            None => "admin".to_string(),
        }
    };
    if caller_role == "unauthorized" || caller_role == "metrics" {
        let status = if caller_role == "unauthorized" {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::FORBIDDEN
        };
        return (
            status,
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32003,
                    "message": "Unauthorized or insufficient token scope for MCP"
                }
            })),
        )
            .into_response();
    }
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;

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
                    "tools": {},
                    "resources": {},
                    "prompts": {}
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
        "resources/list" => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "resources": []
            }
        }))
        .into_response(),
        "resources/templates/list" => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "resourceTemplates": []
            }
        }))
        .into_response(),
        "prompts/list" => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "prompts": []
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
                    let mut parsed_cams: Vec<String> = Vec::new();
                    for key in ["camera", "cameras"] {
                        match args.get(key) {
                            Some(Value::String(s)) => {
                                for part in s.split(',') {
                                    let t = part.trim();
                                    if !t.is_empty() {
                                        parsed_cams.push(t.to_string());
                                    }
                                }
                            }
                            Some(Value::Array(arr)) => {
                                for item in arr {
                                    if let Some(s) = item.as_str() {
                                        let t = s.trim();
                                        if !t.is_empty() {
                                            parsed_cams.push(t.to_string());
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    let req_cams = if parsed_cams.is_empty() {
                        None
                    } else {
                        Some(parsed_cams)
                    };
                    let filter = EventFilter {
                        cameras: scope_cameras_filter(req_cams, allowed),
                        labels: args
                            .get("label")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(|s| vec![s.to_string()]),
                        exclude_labels: None,
                        zones: args
                            .get("zone")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(|s| vec![s.to_string()]),
                        sub_labels: None,
                        plate: args
                            .get("plate")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
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
                    let db_for_mcp = state.db.clone();
                    let mut events =
                        tokio::task::spawn_blocking(move || db_for_mcp.query_events(&filter))
                            .await
                            .ok()
                            .and_then(|r| r.ok())
                            .unwrap_or_default();
                    redact_event_slice_thumbnails_if_masked(&state.config, &mut events).await;
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
                    if !verify_cookie_csrf_origin(&headers, &uri) {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32003,
                                    "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
                                }
                            })),
                        )
                            .into_response();
                    }
                    if caller_role == "viewer"
                        || caller_role == "read_only"
                        || caller_role == "metrics"
                        || caller_role == "unauthorized"
                        || allowed.is_some()
                    {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32003,
                                    "message": "Read-only or camera-restricted role or token cannot modify security profile"
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
                    StatusCode::OK,
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
            StatusCode::OK,
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
