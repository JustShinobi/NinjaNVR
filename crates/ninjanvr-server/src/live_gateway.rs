#![forbid(unsafe_code)]
//! Live `go2rtc` stream routing, camera-scoped RBAC resolution, WebRTC SDP talkback
//! negotiation, stream prewarming, and MSE / WebRTC WebSocket bridging (`LiveStreamGateway`).

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as AxumWsMsg, WebSocket};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use futures_util::stream::BoxStream;
use futures_util::{Sink, SinkExt, StreamExt};
use ninjanvr_config::{
    Go2RtcStreamRoute, NinjaConfig, is_safe_go2rtc_stream_name, parse_go2rtc_stream_route,
    scrub_json_value, scrub_rtsp_credentials,
};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::protocol::Message as TungMsg;
use tracing::warn;

use crate::mse_hub::{self, MseStreamHub};

pub const MAX_GO2RTC_STREAMS_JSON_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_WEBRTC_SDP_BYTES: usize = 256 * 1024;
pub const MAX_GO2RTC_WS_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Response returned from an upstream `go2rtc` HTTP endpoint.
#[derive(Debug, Clone)]
pub struct Go2RtcHttpUpstreamResponse {
    pub status: StatusCode,
    pub content_type: Option<String>,
    pub body: Bytes,
}

/// Boxed sink + stream pair representing an open upstream `go2rtc` WebSocket connection.
pub struct Go2RtcUpstreamWs {
    pub sink: Pin<Box<dyn Sink<TungMsg, Error = String> + Send>>,
    pub stream: BoxStream<'static, Result<TungMsg, String>>,
}

/// Port trait abstracting HTTP & WebSocket exchanges with the local `go2rtc` restreamer (`127.0.0.1:1984`).
pub trait Go2RtcUpstreamPort: Send + Sync {
    fn get_streams_json<'a>(
        &'a self,
        go2rtc_http_base: &'a str,
        single_stream_query: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<Go2RtcHttpUpstreamResponse, String>> + Send + 'a>>;

    fn post_webrtc_sdp<'a>(
        &'a self,
        go2rtc_http_base: &'a str,
        upstream_query: &'a str,
        content_type: &'a str,
        sdp_offer: Bytes,
    ) -> Pin<Box<dyn Future<Output = Result<Go2RtcHttpUpstreamResponse, String>> + Send + 'a>>;

    fn prewarm_mse_stream<'a>(
        &'a self,
        mse_hub: &'a MseStreamHub,
        go2rtc_ws_base: &'a str,
        canonical_stream: &'a str,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            mse_hub
                .prewarm_stream(go2rtc_ws_base, canonical_stream)
                .await
        })
    }

    fn open_upstream_ws<'a>(
        &'a self,
        go2rtc_ws_base: &'a str,
        query: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Go2RtcUpstreamWs, String>> + Send + 'a>>;
}

/// Production `reqwest` + `tokio-tungstenite` adapter for [`Go2RtcUpstreamPort`].
#[derive(Clone)]
pub struct ReqwestGo2RtcUpstream {
    client: reqwest::Client,
}

impl ReqwestGo2RtcUpstream {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

async fn read_bounded_upstream_bytes(
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

fn encode_url_query_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl Go2RtcUpstreamPort for ReqwestGo2RtcUpstream {
    fn get_streams_json<'a>(
        &'a self,
        go2rtc_http_base: &'a str,
        single_stream_query: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<Go2RtcHttpUpstreamResponse, String>> + Send + 'a>> {
        Box::pin(async move {
            let base = go2rtc_http_base.trim_end_matches('/');
            let url = match single_stream_query {
                Some(stream) => {
                    let encoded = encode_url_query_value(stream);
                    format!("{base}/api/streams?src={encoded}&video=all&audio=all&microphone=")
                }
                None => format!("{base}/api/streams"),
            };
            let resp = self
                .client
                .get(&url)
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
            let content_type = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let body = read_bounded_upstream_bytes(resp, MAX_GO2RTC_STREAMS_JSON_BYTES)
                .await
                .ok_or_else(|| "Upstream go2rtc streams JSON exceeded maximum size".to_string())?;
            Ok(Go2RtcHttpUpstreamResponse {
                status,
                content_type,
                body,
            })
        })
    }

    fn post_webrtc_sdp<'a>(
        &'a self,
        go2rtc_http_base: &'a str,
        upstream_query: &'a str,
        content_type: &'a str,
        sdp_offer: Bytes,
    ) -> Pin<Box<dyn Future<Output = Result<Go2RtcHttpUpstreamResponse, String>> + Send + 'a>> {
        Box::pin(async move {
            let base = go2rtc_http_base.trim_end_matches('/');
            let url = if upstream_query.is_empty() {
                format!("{base}/api/webrtc")
            } else {
                format!("{base}/api/webrtc?{upstream_query}")
            };
            let resp = self
                .client
                .post(&url)
                .timeout(Duration::from_secs(5))
                .header(header::CONTENT_TYPE, content_type)
                .body(sdp_offer.to_vec())
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
            let resp_ct = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let body = read_bounded_upstream_bytes(resp, MAX_WEBRTC_SDP_BYTES)
                .await
                .ok_or_else(|| {
                    "Upstream go2rtc WebRTC response exceeded maximum size".to_string()
                })?;
            Ok(Go2RtcHttpUpstreamResponse {
                status,
                content_type: resp_ct,
                body,
            })
        })
    }

    fn open_upstream_ws<'a>(
        &'a self,
        go2rtc_ws_base: &'a str,
        query: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Go2RtcUpstreamWs, String>> + Send + 'a>> {
        Box::pin(async move {
            let base = go2rtc_ws_base.trim_end_matches('/');
            let target_url = if query.is_empty() {
                format!("{base}/api/ws")
            } else {
                format!("{base}/api/ws?{query}")
            };
            let ws_cfg = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                .max_message_size(Some(MAX_GO2RTC_WS_MESSAGE_BYTES))
                .max_frame_size(Some(MAX_GO2RTC_WS_MESSAGE_BYTES));
            let (upstream_ws, _) = tokio::time::timeout(
                Duration::from_secs(5),
                tokio_tungstenite::connect_async_with_config(&target_url, Some(ws_cfg), false),
            )
            .await
            .map_err(|_| format!("Timed out connecting upstream go2rtc WS at {target_url}"))?
            .map_err(|e| format!("Failed to connect upstream go2rtc WS at {target_url}: {e}"))?;
            let (up_sink, up_stream) = upstream_ws.split();
            Ok(Go2RtcUpstreamWs {
                sink: Box::pin(up_sink.sink_map_err(|e| e.to_string())),
                stream: up_stream.map(|r| r.map_err(|e| e.to_string())).boxed(),
            })
        })
    }
}

/// Returns `true` when `query` injects a `dst` consumer parameter or semicolon separator.
pub fn query_has_disallowed_go2rtc_dst(query: &str) -> bool {
    query.contains(';')
        || query.split('&').any(|kv| {
            let raw_k = kv.split('=').next().unwrap_or(kv).trim();
            raw_k.eq_ignore_ascii_case("dst")
                || crate::auth_gate::percent_decode_segment(raw_k)
                    .is_some_and(|dk| dk.eq_ignore_ascii_case("dst"))
        })
}

fn is_audio_media_rest(media_rest: &str) -> bool {
    media_rest
        .trim_start()
        .strip_prefix("audio")
        .is_some_and(|r| r.is_empty() || r.starts_with(char::is_whitespace))
}

fn is_video_media_rest(media_rest: &str) -> bool {
    media_rest
        .trim_start()
        .strip_prefix("video")
        .is_some_and(|r| r.is_empty() || r.starts_with(char::is_whitespace))
}

/// Detects an audio-only WebRTC SDP offer carrying a send-capable microphone backchannel
/// (`sendonly` or `sendrecv`, without a `video` media section).
pub fn is_webrtc_audio_talkback_offer(sdp_bytes: &[u8]) -> bool {
    let Ok(sdp) = std::str::from_utf8(sdp_bytes) else {
        return false;
    };
    let mut has_audio = false;
    for raw_line in sdp.split(['\r', '\n']) {
        let line_lc = raw_line.trim().to_ascii_lowercase();
        if let Some(media_rest) = line_lc.strip_prefix("m=") {
            if is_video_media_rest(media_rest) {
                return false;
            }
            if is_audio_media_rest(media_rest) {
                has_audio = true;
            }
        }
    }
    has_audio && sdp_has_send_capable_audio(sdp)
}

/// Returns `true` when an SDP string contains any `m=audio` section that could transmit audio
/// (`pion/webrtc` defaults to `sendrecv` when omitted, splits `a=<key>:<value>` on `:` while
/// checking `attr.Key`, and honors the first direction attribute; therefore an `m=audio` section
/// is safe for read-only callers ONLY when it declares exactly one canonical `a=recvonly` or
/// `a=inactive` line and zero `sendonly`/`sendrecv`/suffixed direction attributes).
pub fn sdp_has_send_capable_audio(sdp: &str) -> bool {
    let mut in_media = false;
    let mut in_audio = false;
    let mut session_has_send_dir = false;
    let mut audio_recv_or_inactive_count = 0usize;
    let mut audio_send_dir_count = 0usize;

    let section_is_send_capable = |recv_cnt: usize, send_cnt: usize, sess_send: bool| -> bool {
        send_cnt > 0 || recv_cnt != 1 || (recv_cnt == 0 && sess_send)
    };

    for raw_line in sdp.split(['\r', '\n']) {
        let line = raw_line.trim().to_ascii_lowercase();
        if line.is_empty() {
            continue;
        }
        if let Some(media_rest) = line.strip_prefix("m=") {
            if in_audio
                && section_is_send_capable(
                    audio_recv_or_inactive_count,
                    audio_send_dir_count,
                    session_has_send_dir,
                )
            {
                return true;
            }
            in_media = true;
            in_audio = is_audio_media_rest(media_rest);
            audio_recv_or_inactive_count = 0;
            audio_send_dir_count = 0;
            continue;
        }
        let attr_key = line
            .strip_prefix("a=")
            .map(|rest| rest.split(':').next().unwrap_or(rest).trim());
        if !in_media {
            if matches!(attr_key, Some("sendonly" | "sendrecv")) {
                session_has_send_dir = true;
            }
            continue;
        }
        if in_audio {
            if line == "a=recvonly" || line == "a=inactive" {
                audio_recv_or_inactive_count += 1;
            } else if matches!(
                attr_key,
                Some("sendonly" | "sendrecv" | "recvonly" | "inactive")
            ) {
                audio_send_dir_count += 1;
            }
        }
    }
    in_audio
        && section_is_send_capable(
            audio_recv_or_inactive_count,
            audio_send_dir_count,
            session_has_send_dir,
        )
}

/// Strict allowlist validator for client-to-`go2rtc` WebSocket control frames from read-only
/// (`viewer` / `read_only`) callers, neutralizing Go `encoding/json` case-insensitive field
/// matching (`Type`/`Value`/`SDP`) and blocking non-recvonly WebRTC audio offers.
pub fn is_permitted_readonly_go2rtc_ws_message(raw_msg: &str) -> bool {
    let trimmed = raw_msg.trim();
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) else {
        return false;
    };
    let mut type_val: Option<&str> = None;
    let mut value_entry: Option<&Value> = None;
    for (k, v) in &map {
        if k.eq_ignore_ascii_case("type") {
            if type_val.is_some() {
                return false;
            }
            let Some(s) = v.as_str() else {
                return false;
            };
            type_val = Some(s);
        } else if k.eq_ignore_ascii_case("value") {
            if value_entry.is_some() {
                return false;
            }
            value_entry = Some(v);
        } else {
            return false;
        }
    }
    let Some(msg_ty) = type_val.map(|s| s.trim().to_ascii_lowercase()) else {
        return false;
    };
    match msg_ty.as_str() {
        "mse" | "webrtc/candidate" => true,
        "webrtc" | "webrtc/offer" => {
            let Some(val) = value_entry else {
                return false;
            };
            match val {
                Value::String(sdp) => !sdp_has_send_capable_audio(sdp),
                Value::Object(inner) => {
                    let mut sdp_str: Option<&str> = None;
                    for (ik, iv) in inner {
                        if ik.eq_ignore_ascii_case("sdp") {
                            if sdp_str.is_some() {
                                return false;
                            }
                            let Some(s) = iv.as_str() else {
                                return false;
                            };
                            sdp_str = Some(s);
                        } else if !ik.eq_ignore_ascii_case("type") {
                            return false;
                        }
                    }
                    sdp_str.is_some_and(|sdp| !sdp_has_send_capable_audio(sdp))
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// Inspects a WebSocket client text message (such as `go2rtc` JSON `{"type":"webrtc","value":"v=0..."}`
/// or raw SDP) and returns `true` if it attempts to negotiate a send-capable audio backchannel
/// or uses a non-allowlisted control frame shape for read-only callers.
pub fn ws_message_contains_webrtc_talkback(raw_msg: &str) -> bool {
    let trimmed = raw_msg.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with('{') {
        return !is_permitted_readonly_go2rtc_ws_message(trimmed);
    }
    if trimmed.to_ascii_lowercase().contains("m=") {
        return sdp_has_send_capable_audio(trimmed);
    }
    true
}

/// Rewrites the first `src=...` query parameter in `query` to `new_src`.
pub fn rewrite_query_src_param(query: &str, new_src: &str) -> String {
    let mut parts = Vec::new();
    let mut replaced = false;
    for kv in query.split('&') {
        if kv.is_empty() {
            continue;
        }
        let (raw_k, _) = kv.split_once('=').unwrap_or((kv, ""));
        if !replaced && raw_k.eq_ignore_ascii_case("src") {
            parts.push(format!("src={new_src}"));
            replaced = true;
        } else {
            parts.push(kv.to_string());
        }
    }
    parts.join("&")
}

/// Deep gateway module coordinating `go2rtc` stream routing, camera-scoped RBAC, WebRTC
/// talkback stream isolation, MSE prewarming, and WebSocket bridging.
#[derive(Clone)]
pub struct LiveStreamGateway {
    config: Arc<RwLock<NinjaConfig>>,
    mse_hub: MseStreamHub,
    upstream: Arc<dyn Go2RtcUpstreamPort>,
}

impl LiveStreamGateway {
    pub fn new(
        config: Arc<RwLock<NinjaConfig>>,
        mse_hub: MseStreamHub,
        upstream: Arc<dyn Go2RtcUpstreamPort>,
    ) -> Self {
        Self {
            config,
            mse_hub,
            upstream,
        }
    }

    /// Snapshots the set of configured camera identifiers for exact stream-owner resolution.
    pub async fn known_camera_set(&self) -> BTreeSet<String> {
        self.config.read().await.cameras.keys().cloned().collect()
    }

    /// Parses `stream` into its canonical [`Go2RtcStreamRoute`] against configured cameras.
    pub async fn resolve_stream_route(&self, stream: &str) -> Option<Go2RtcStreamRoute> {
        let known = self.known_camera_set().await;
        parse_go2rtc_stream_route(stream, Some(&known))
    }

    /// Checks whether `stream` is syntactically valid and permitted under `allowed_cameras`.
    pub async fn authorize_stream(
        &self,
        stream: &str,
        allowed_cameras: Option<&[String]>,
    ) -> Result<Go2RtcStreamRoute, (StatusCode, &'static str)> {
        if !is_safe_go2rtc_stream_name(stream) {
            return Err((StatusCode::BAD_REQUEST, "Invalid stream identifier"));
        }
        let Some(route) = self.resolve_stream_route(stream).await else {
            return Err((StatusCode::BAD_REQUEST, "Invalid stream identifier"));
        };
        if let Some(allow_list) = allowed_cameras
            && !allow_list.iter().any(|a| a == &route.camera || a == "*")
        {
            return Err((
                StatusCode::FORBIDDEN,
                "Stream camera is not permitted for this user",
            ));
        }
        Ok(route)
    }

    /// Lists active `go2rtc` streams scrubbed of credentials and filtered by `allowed_cameras`
    /// across `{cam}`, `{cam}_sub`, `{cam}_talk`, and `{cam}_sub_talk`.
    pub async fn list_streams(
        &self,
        go2rtc_http_base: &str,
        allowed_cameras: Option<&[String]>,
    ) -> Response {
        let known = self.known_camera_set().await;
        match self.upstream.get_streams_json(go2rtc_http_base, None).await {
            Ok(resp) => {
                if let Ok(json_val) = serde_json::from_slice::<Value>(&resp.body) {
                    let mut scrubbed = scrub_json_value(&json_val);
                    if let Some(allow_list) = allowed_cameras
                        && let Some(obj) = scrubbed.as_object_mut()
                    {
                        obj.retain(|key, _| {
                            parse_go2rtc_stream_route(key, Some(&known)).is_some_and(|route| {
                                allow_list.iter().any(|a| a == &route.camera || a == "*")
                            })
                        });
                    }
                    return (resp.status, Json(scrubbed)).into_response();
                }
                Json(json!({})).into_response()
            }
            Err(_) => Json(json!({})).into_response(),
        }
    }

    /// Alias for [`Self::list_streams`].
    pub async fn list_permitted_streams(
        &self,
        go2rtc_http_base: &str,
        allowed_cameras: Option<&[String]>,
    ) -> Response {
        self.list_streams(go2rtc_http_base, allowed_cameras).await
    }

    /// Fetches metadata for a single `go2rtc` stream after enforcing camera-scoped RBAC.
    pub async fn get_stream_one(
        &self,
        go2rtc_http_base: &str,
        stream: &str,
        allowed_cameras: Option<&[String]>,
    ) -> Response {
        if let Err((status, msg)) = self.authorize_stream(stream, allowed_cameras).await {
            return (status, Json(json!({ "success": false, "message": msg }))).into_response();
        }
        match self
            .upstream
            .get_streams_json(go2rtc_http_base, Some(stream))
            .await
        {
            Ok(resp) => {
                if let Ok(json_val) = serde_json::from_slice::<Value>(&resp.body) {
                    return (resp.status, Json(scrub_json_value(&json_val))).into_response();
                }
                Json(json!({})).into_response()
            }
            Err(_) => Json(json!({})).into_response(),
        }
    }

    /// Alias for [`Self::get_stream_one`].
    pub async fn get_permitted_stream(
        &self,
        go2rtc_http_base: &str,
        stream: &str,
        allowed_cameras: Option<&[String]>,
    ) -> Response {
        self.get_stream_one(go2rtc_http_base, stream, allowed_cameras)
            .await
    }

    /// Prewarms the shared MSE GOP cache for `stream` (resolving loopback `_sub` aliases).
    pub async fn prewarm_stream(
        &self,
        go2rtc_ws_base: &str,
        stream: &str,
        allowed_cameras: Option<&[String]>,
    ) -> Response {
        if let Err((status, msg)) = self.authorize_stream(stream, allowed_cameras).await {
            return (status, Json(json!({ "success": false, "message": msg }))).into_response();
        }
        let canonical_stream = {
            let cfg = self.config.read().await;
            cfg.resolve_mse_stream_alias(stream).to_string()
        };
        let warm = self
            .upstream
            .prewarm_mse_stream(&self.mse_hub, go2rtc_ws_base, &canonical_stream)
            .await;
        Json(json!({
            "success": true,
            "stream": stream,
            "warm": warm,
        }))
        .into_response()
    }

    /// Negotiates a WebRTC SDP offer against `go2rtc`, enforcing camera-scoped RBAC, blocking
    /// `dst=` consumer injection, and transparently routing audio-only talkback offers onto
    /// `{camera}_talk` when a dedicated backchannel stream is configured.
    pub async fn negotiate_webrtc_sdp(
        &self,
        go2rtc_http_base: &str,
        raw_query: Option<&str>,
        headers: &HeaderMap,
        body: Bytes,
        allowed_cameras: Option<&[String]>,
    ) -> Response {
        if body.len() > MAX_WEBRTC_SDP_BYTES {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({
                    "success": false,
                    "message": "WebRTC SDP offer exceeds maximum allowed size"
                })),
            )
                .into_response();
        }
        let known = self.known_camera_set().await;
        if let Err(msg) = crate::auth_gate::validate_go2rtc_query_streams_with_known(
            raw_query,
            allowed_cameras,
            Some(&known),
        ) {
            let status = if msg == "Stream camera is not permitted for this user" {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::BAD_REQUEST
            };
            return (status, Json(json!({ "success": false, "message": msg }))).into_response();
        }
        let query = raw_query.unwrap_or("");
        let Some(raw_src) = (!query_has_disallowed_go2rtc_dst(query))
            .then(|| mse_hub::extract_stream_src_from_query(query))
            .flatten()
        else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "success": false,
                    "message": "Invalid or ambiguous stream src query parameter"
                })),
            )
                .into_response();
        };

        let upstream_query = {
            let cfg = self.config.read().await;
            let owner_cam = cfg
                .resolve_stream_owner_camera(&raw_src)
                .unwrap_or(raw_src.as_str());
            let target_talk =
                if !raw_src.ends_with("_talk") && is_webrtc_audio_talkback_offer(&body) {
                    if cfg.has_dedicated_talk_stream(owner_cam) {
                        Some(format!("{owner_cam}_talk"))
                    } else if cfg.has_dedicated_talk_stream(&raw_src) {
                        Some(format!("{raw_src}_talk"))
                    } else {
                        None
                    }
                } else {
                    None
                };
            if let Some(talk_src) = target_talk {
                rewrite_query_src_param(query, &talk_src)
            } else {
                query.to_string()
            }
        };

        let req_ct = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/sdp");

        match self
            .upstream
            .post_webrtc_sdp(go2rtc_http_base, &upstream_query, req_ct, body)
            .await
        {
            Ok(resp) => {
                let content_type = resp
                    .content_type
                    .unwrap_or_else(|| "application/sdp".to_string());
                let body_out = if let Ok(text) = std::str::from_utf8(&resp.body) {
                    Bytes::from(scrub_rtsp_credentials(text))
                } else {
                    resp.body
                };
                (
                    resp.status,
                    [(header::CONTENT_TYPE, content_type)],
                    Body::from(body_out),
                )
                    .into_response()
            }
            Err(err) => (StatusCode::BAD_GATEWAY, scrub_rtsp_credentials(&err)).into_response(),
        }
    }

    /// Bridges an upgraded client WebSocket either onto `MseStreamHub::pump_client_ws` (for MSE
    /// streams) or a direct credential-scrubbing upstream `go2rtc` WebSocket relay (for WebRTC WS).
    pub async fn bridge_ws(
        &self,
        client_ws: WebSocket,
        go2rtc_ws_base: &str,
        query: String,
        is_webrtc_ws: bool,
    ) {
        self.bridge_ws_with_talkback_guard(client_ws, go2rtc_ws_base, query, is_webrtc_ws, true)
            .await;
    }

    /// Bridges an upgraded client WebSocket while enforcing talkback RBAC (`caller_can_talkback`)
    /// so read-only `viewer` / `read_only` callers cannot transmit microphone audio over WebRTC WS.
    pub async fn bridge_ws_with_talkback_guard(
        &self,
        client_ws: WebSocket,
        go2rtc_ws_base: &str,
        query: String,
        is_webrtc_ws: bool,
        caller_can_talkback: bool,
    ) {
        if !caller_can_talkback
            && let Some(raw_src) = mse_hub::extract_stream_src_from_query(&query)
            && matches!(
                self.resolve_stream_route(&raw_src).await.map(|r| r.variant),
                Some(
                    ninjanvr_config::Go2RtcStreamVariant::Talkback
                        | ninjanvr_config::Go2RtcStreamVariant::SubTalkback
                )
            )
        {
            return;
        }
        if !is_webrtc_ws && let Some(raw_src) = mse_hub::extract_stream_src_from_query(&query) {
            let resolved_src = {
                let cfg = self.config.read().await;
                cfg.resolve_mse_stream_alias(&raw_src).to_string()
            };
            self.mse_hub
                .pump_client_ws(client_ws, go2rtc_ws_base, &resolved_src)
                .await;
            return;
        }

        let Ok(Go2RtcUpstreamWs {
            sink: mut up_sink,
            stream: mut up_stream,
        }) = self.upstream.open_upstream_ws(go2rtc_ws_base, &query).await
        else {
            warn!(
                "Failed to connect upstream go2rtc WS at {}/api/ws",
                scrub_rtsp_credentials(go2rtc_ws_base)
            );
            return;
        };

        let (mut client_sink, mut client_stream) = client_ws.split();

        let c2u = async {
            while let Some(Ok(msg)) = client_stream.next().await {
                let out = match msg {
                    AxumWsMsg::Text(t) => {
                        if t.len() > MAX_GO2RTC_WS_MESSAGE_BYTES {
                            break;
                        }
                        if !caller_can_talkback && ws_message_contains_webrtc_talkback(&t) {
                            break;
                        }
                        TungMsg::Text(t.to_string().into())
                    }
                    AxumWsMsg::Binary(b) => {
                        if !caller_can_talkback || b.len() > MAX_GO2RTC_WS_MESSAGE_BYTES {
                            break;
                        }
                        TungMsg::Binary(b)
                    }
                    AxumWsMsg::Ping(p) => TungMsg::Ping(p),
                    AxumWsMsg::Pong(p) => TungMsg::Pong(p),
                    AxumWsMsg::Close(_) => break,
                };
                if !matches!(
                    tokio::time::timeout(Duration::from_secs(5), up_sink.send(out)).await,
                    Ok(Ok(()))
                ) {
                    break;
                }
            }
        };

        let u2c = async {
            while let Some(Ok(msg)) = up_stream.next().await {
                let out = match msg {
                    TungMsg::Text(t) => {
                        if t.len() > MAX_GO2RTC_WS_MESSAGE_BYTES {
                            break;
                        }
                        AxumWsMsg::Text(scrub_rtsp_credentials(&t).into())
                    }
                    TungMsg::Binary(b) => {
                        if b.len() > MAX_GO2RTC_WS_MESSAGE_BYTES {
                            break;
                        }
                        AxumWsMsg::Binary(b)
                    }
                    TungMsg::Ping(p) => AxumWsMsg::Ping(p),
                    TungMsg::Pong(p) => AxumWsMsg::Pong(p),
                    TungMsg::Close(_) => break,
                    TungMsg::Frame(_) => continue,
                };
                if !matches!(
                    tokio::time::timeout(Duration::from_secs(5), client_sink.send(out)).await,
                    Ok(Ok(()))
                ) {
                    break;
                }
            }
        };

        tokio::select! {
            _ = c2u => {},
            _ = u2c => {},
        }
    }
}

/// In-memory test adapter for [`Go2RtcUpstreamPort`] allowing deterministic RBAC, credential-scrubbing,
/// prewarm, WebSocket relay, and WebRTC talkback routing tests without spawning TCP listeners.
#[derive(Default)]
pub struct InMemoryGo2RtcUpstream {
    pub streams_payload: std::sync::Mutex<Value>,
    pub captured_webrtc_queries: std::sync::Mutex<Vec<(String, String, String)>>,
    pub captured_prewarms: std::sync::Mutex<Vec<String>>,
    pub prewarm_result: std::sync::atomic::AtomicBool,
    pub webrtc_answer_sdp: std::sync::Mutex<String>,
    pub canned_ws_messages: std::sync::Mutex<Vec<TungMsg>>,
    pub captured_ws_client_messages: Arc<std::sync::Mutex<Vec<(String, TungMsg)>>>,
}

impl InMemoryGo2RtcUpstream {
    pub fn with_streams(streams_payload: Value) -> Self {
        Self {
            streams_payload: std::sync::Mutex::new(streams_payload),
            captured_webrtc_queries: std::sync::Mutex::new(Vec::new()),
            captured_prewarms: std::sync::Mutex::new(Vec::new()),
            prewarm_result: std::sync::atomic::AtomicBool::new(true),
            webrtc_answer_sdp: std::sync::Mutex::new(
                "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=fake\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\n"
                    .to_string(),
            ),
            canned_ws_messages: std::sync::Mutex::new(Vec::new()),
            captured_ws_client_messages: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

impl Go2RtcUpstreamPort for InMemoryGo2RtcUpstream {
    fn get_streams_json<'a>(
        &'a self,
        _go2rtc_http_base: &'a str,
        single_stream_query: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<Go2RtcHttpUpstreamResponse, String>> + Send + 'a>> {
        Box::pin(async move {
            let full = self
                .streams_payload
                .lock()
                .map_or_else(|_| json!({}), |g| g.clone());
            let payload = if let Some(target) = single_stream_query {
                full.get(target).cloned().unwrap_or_else(|| json!({}))
            } else {
                full
            };
            Ok(Go2RtcHttpUpstreamResponse {
                status: StatusCode::OK,
                content_type: Some("application/json".to_string()),
                body: Bytes::from(serde_json::to_vec(&payload).unwrap_or_default()),
            })
        })
    }

    fn post_webrtc_sdp<'a>(
        &'a self,
        _go2rtc_http_base: &'a str,
        upstream_query: &'a str,
        content_type: &'a str,
        sdp_offer: Bytes,
    ) -> Pin<Box<dyn Future<Output = Result<Go2RtcHttpUpstreamResponse, String>> + Send + 'a>> {
        Box::pin(async move {
            if let Ok(mut guard) = self.captured_webrtc_queries.lock() {
                guard.push((
                    upstream_query.to_string(),
                    content_type.to_string(),
                    String::from_utf8_lossy(&sdp_offer).to_string(),
                ));
            }
            let answer = self
                .webrtc_answer_sdp
                .lock()
                .map_or_else(|_| String::new(), |g| g.clone());
            Ok(Go2RtcHttpUpstreamResponse {
                status: StatusCode::OK,
                content_type: Some("application/sdp".to_string()),
                body: Bytes::from(answer),
            })
        })
    }

    fn prewarm_mse_stream<'a>(
        &'a self,
        _mse_hub: &'a MseStreamHub,
        _go2rtc_ws_base: &'a str,
        canonical_stream: &'a str,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            if let Ok(mut guard) = self.captured_prewarms.lock() {
                guard.push(canonical_stream.to_string());
            }
            self.prewarm_result
                .load(std::sync::atomic::Ordering::Relaxed)
        })
    }

    fn open_upstream_ws<'a>(
        &'a self,
        _go2rtc_ws_base: &'a str,
        query: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Go2RtcUpstreamWs, String>> + Send + 'a>> {
        Box::pin(async move {
            let canned = self
                .canned_ws_messages
                .lock()
                .map_or_else(|_| Vec::new(), |g| g.clone());
            let query_owned = query.to_string();
            let captured = self.captured_ws_client_messages.clone();
            let sink = futures_util::sink::unfold(
                (query_owned, captured),
                |(q, cap), msg: TungMsg| async move {
                    if let Ok(mut guard) = cap.lock() {
                        guard.push((q.clone(), msg));
                    }
                    Ok::<_, String>((q, cap))
                },
            );
            let stream = futures_util::stream::iter(canned.into_iter().map(Ok)).boxed();
            Ok(Go2RtcUpstreamWs {
                sink: Box::pin(sink),
                stream,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use std::path::PathBuf;

    #[tokio::test]
    async fn live_stream_gateway_permits_sub_talk_and_sub_talkback_for_scoped_camera_user() {
        let yaml = r#"
go2rtc:
  streams:
    quarto:
      - "rtsp://camuser:secretpass@192.0.2.10:554/stream1"
      - "tapo://cloudsecret@192.0.2.10"
    quarto_sub:
      - "rtsp://camuser:secretpass@192.0.2.10:554/stream2"
      - "tapo://cloudsecret@192.0.2.10"
    sala: "rtsp://camuser:secretpass@192.0.2.11:554/stream1"
cameras:
  quarto:
    enabled: true
  sala:
    enabled: true
"#;
        let cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        let fake = Arc::new(InMemoryGo2RtcUpstream::with_streams(json!({
            "quarto": { "producers": [{ "url": "rtsp://camuser:secretpass@192.0.2.10:554/stream1" }] },
            "quarto_sub": { "producers": [{ "url": "rtsp://camuser:secretpass@192.0.2.10:554/stream2" }] },
            "quarto_talk": { "producers": [{ "url": "tapo://cloudsecret@192.0.2.10" }] },
            "quarto_sub_talk": { "producers": [{ "url": "tapo://cloudsecret@192.0.2.10" }] },
            "sala": { "producers": [{ "url": "rtsp://camuser:secretpass@192.0.2.11:554/stream1" }] },
            "sala_talk": { "producers": [{ "url": "tapo://cloudsecret@192.0.2.11" }] },
        })));
        let gw = LiveStreamGateway::new(
            Arc::new(RwLock::new(cfg)),
            MseStreamHub::new(),
            fake.clone(),
        );

        let allowed = vec!["quarto".to_string()];

        // 1. Listing streams retains quarto, quarto_sub, quarto_talk, and quarto_sub_talk, scrubs secrets, and hides sala*
        let list_resp = gw
            .list_permitted_streams("http://127.0.0.1:1984", Some(&allowed))
            .await;
        assert_eq!(list_resp.status(), StatusCode::OK);
        let list_bytes = to_bytes(list_resp.into_body(), 1024 * 1024).await.unwrap();
        let list_json: Value = serde_json::from_slice(&list_bytes).unwrap();
        let obj = list_json.as_object().unwrap();
        assert!(obj.contains_key("quarto"));
        assert!(obj.contains_key("quarto_sub"));
        assert!(obj.contains_key("quarto_talk"));
        assert!(obj.contains_key("quarto_sub_talk"));
        assert!(!obj.contains_key("sala"));
        assert!(!obj.contains_key("sala_talk"));
        let serialized = serde_json::to_string(&list_json).unwrap();
        assert!(!serialized.contains("secretpass"));
        assert!(!serialized.contains("cloudsecret"));

        // 2. Single-stream fetch & prewarm allow quarto_talk & quarto_sub_talk, reject sala_talk with 403
        for ok_stream in ["quarto", "quarto_sub", "quarto_talk", "quarto_sub_talk"] {
            let r = gw
                .get_permitted_stream("http://127.0.0.1:1984", ok_stream, Some(&allowed))
                .await;
            assert_eq!(r.status(), StatusCode::OK, "stream={ok_stream}");

            let pw = gw
                .prewarm_stream("ws://127.0.0.1:1984", ok_stream, Some(&allowed))
                .await;
            assert_eq!(pw.status(), StatusCode::OK, "prewarm stream={ok_stream}");
        }
        let forbidden = gw
            .get_permitted_stream("http://127.0.0.1:1984", "sala_talk", Some(&allowed))
            .await;
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
        let forbidden_pw = gw
            .prewarm_stream("ws://127.0.0.1:1984", "sala_sub_talk", Some(&allowed))
            .await;
        assert_eq!(forbidden_pw.status(), StatusCode::FORBIDDEN);

        // 3. Audio-only talkback SDP offer on src=quarto_sub reroutes upstream to src=quarto_talk,
        //    whereas recvonly audio + video viewer SDP offer stays on src=quarto_sub!
        let talk_sdp = Bytes::from_static(
            b"v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\n",
        );
        let sdp_resp = gw
            .negotiate_webrtc_sdp(
                "http://127.0.0.1:1984",
                Some("src=quarto_sub"),
                &HeaderMap::new(),
                talk_sdp.clone(),
                Some(&allowed),
            )
            .await;
        assert_eq!(sdp_resp.status(), StatusCode::OK);

        let viewer_sdp = Bytes::from_static(
            b"v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=recvonly\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendrecv\r\n",
        );
        let viewer_resp = gw
            .negotiate_webrtc_sdp(
                "http://127.0.0.1:1984",
                Some("src=quarto_sub"),
                &HeaderMap::new(),
                viewer_sdp,
                Some(&allowed),
            )
            .await;
        assert_eq!(viewer_resp.status(), StatusCode::OK);

        // Forbidden camera talkback SDP offer returns 403 without hitting upstream
        let forbidden_sdp = gw
            .negotiate_webrtc_sdp(
                "http://127.0.0.1:1984",
                Some("src=sala_talk"),
                &HeaderMap::new(),
                talk_sdp,
                Some(&allowed),
            )
            .await;
        assert_eq!(forbidden_sdp.status(), StatusCode::FORBIDDEN);

        let queries = fake.captured_webrtc_queries.lock().unwrap().clone();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].0, "src=quarto_talk");
        assert_eq!(queries[1].0, "src=quarto_sub");
    }
}
