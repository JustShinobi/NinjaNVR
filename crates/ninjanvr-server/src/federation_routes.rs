#![forbid(unsafe_code)]
//! Multi-site federation peer proxy (`/api/federation/*`) and admin camera WebUI proxy
//! (`/api/camera-proxy/*`) with SSRF, DNS-rebinding, env-secret, and credential-scrubbing guards.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Json, Response};
use axum_extra::extract::cookie::CookieJar;
use ninjanvr_config::{
    NINJANVR_VERSION, is_safe_secret_env_var_name, scrub_json_value, scrub_rtsp_credentials,
    scrub_yaml_credentials,
};
use serde_json::{Value, json};

use crate::AppState;
use crate::auth_gate::{
    authorize_admin_read, bare_hostname, caller_allowed_cameras, caller_can_mutate,
    extract_url_scheme_and_host, percent_decode_path, read_upstream_body_bounded,
    verify_cookie_csrf_origin,
};

pub const DISALLOWED_FEDERATION_PORTS: &[u16] = &[0, 1984, 5000, 8554, 8555, 9091];

pub fn is_allowed_peer_token_env(name: &str) -> bool {
    let trimmed = name.trim();
    is_safe_secret_env_var_name(trimmed) && trimmed.starts_with("NINJANVR_PEER_")
}

pub fn is_disallowed_federation_port(port: u16) -> bool {
    if DISALLOWED_FEDERATION_PORTS.contains(&port) {
        return true;
    }
    for env_key in ["NINJANVR_HTTP_PORT", "NINJANVR_METRICS_PORT"] {
        if let Ok(raw) = std::env::var(env_key)
            && let Ok(env_port) = raw.trim().parse::<u16>()
            && env_port > 0
            && env_port != 8971
            && env_port == port
        {
            return true;
        }
    }
    false
}

/// Blocks loopback (`127.0.0.0/8`, `::1`), unspecified (`0.0.0.0/8`, `::`), link-local / cloud
/// metadata (`169.254.0.0/16`, `fe80::/10`, `fd00:ec2::254`, `100.100.100.200`), multicast,
/// broadcast, and IPv6 transition tunnels (`NAT64` `64:ff9b::`, `6to4` `2002::/16`, `Teredo` `2001:0000::/32`).
pub fn is_disallowed_federation_peer_ip(ip: IpAddr) -> bool {
    is_disallowed_federation_peer_ip_inner(ip, false)
}

fn is_disallowed_federation_peer_ip_inner(ip: IpAddr, allow_ephemeral_loopback: bool) -> bool {
    let canon = match ip {
        IpAddr::V6(v6) => {
            let segs = v6.segments();
            // NAT64 (64:ff9b::/96 and 64:ff9b:1::/48), 6to4 (2002::/16), Teredo (2001:0000::/32)
            if (segs[0] == 0x0064 && segs[1] == 0xff9b)
                || segs[0] == 0x2002
                || (segs[0] == 0x2001 && segs[1] == 0x0000)
            {
                return true;
            }
            if let Some(v4) = v6.to_ipv4() {
                IpAddr::V4(v4)
            } else {
                ip.to_canonical()
            }
        }
        _ => ip.to_canonical(),
    };
    match canon {
        IpAddr::V4(v4) => {
            let octs = v4.octets();
            (!allow_ephemeral_loopback && v4.is_loopback())
                || v4.is_unspecified()
                || octs[0] == 0
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || octs == [100, 100, 100, 200]
        }
        IpAddr::V6(v6) => {
            let segs = v6.segments();
            (!allow_ephemeral_loopback && v6.is_loopback())
                || v6.is_unspecified()
                || v6.is_unicast_link_local()
                || v6.is_multicast()
                || (segs[0] == 0xfd00 && segs[1] == 0x0ec2 && segs[7] == 0x0254)
        }
    }
}

fn is_disallowed_federation_hostname(host_lc: &str) -> bool {
    host_lc.is_empty()
        || host_lc == "localhost"
        || host_lc.ends_with(".localhost")
        || host_lc == "ninjanvr"
        || host_lc == "go2rtc"
        || host_lc.ends_with(".svc")
        || host_lc.ends_with(".cluster.local")
        || host_lc.ends_with(".internal")
        || host_lc == "metadata.google.internal"
        || host_lc.ends_with(".metadata.google.internal")
}

pub fn is_safe_federation_peer_base_url(raw_url: &str) -> bool {
    is_safe_federation_peer_base_url_inner(raw_url, false)
}

fn is_safe_federation_peer_base_url_inner(raw_url: &str, allow_ephemeral_loopback: bool) -> bool {
    let trimmed = raw_url.trim().trim_end_matches('/');
    if trimmed.is_empty()
        || trimmed.contains("..")
        || trimmed.to_ascii_lowercase().contains("%2e")
        || trimmed.contains(['\\', '?', '#'])
        || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return false;
    }
    let Some((_, host_port)) = extract_url_scheme_and_host(trimmed) else {
        return false;
    };
    let host_port_no_auth = host_port.rsplit_once('@').map_or(host_port, |(_, h)| h);
    if host_port_no_auth.ends_with(":0") {
        return false;
    }
    let host = bare_hostname(host_port_no_auth)
        .trim()
        .trim_end_matches('.');
    let host_lc = host.to_ascii_lowercase();
    if host.is_empty() || host.contains(['@', '%']) || is_disallowed_federation_hostname(&host_lc) {
        return false;
    }
    if let Ok(ip) = host.parse::<IpAddr>()
        && is_disallowed_federation_peer_ip_inner(ip, allow_ephemeral_loopback)
    {
        return false;
    }
    let Ok(parsed_url) = reqwest::Url::parse(trimmed) else {
        return false;
    };
    let Some(effective_port) = parsed_url.port_or_known_default() else {
        return false;
    };
    if parsed_url.port() == Some(0) || is_disallowed_federation_port(effective_port) {
        return false;
    }
    let Some(raw_parsed_host) = parsed_url.host_str() else {
        return false;
    };
    let clean_parsed_host = raw_parsed_host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.');
    if clean_parsed_host.is_empty() {
        return false;
    }
    if let Ok(ip) = clean_parsed_host.parse::<IpAddr>() {
        if is_disallowed_federation_peer_ip_inner(ip, allow_ephemeral_loopback) {
            return false;
        }
    } else if is_disallowed_federation_hostname(&clean_parsed_host.to_ascii_lowercase()) {
        return false;
    }
    true
}

/// Resolves DNS (or literal socket IP) for `base_url`, verifies every resolved `SocketAddr`
/// against `is_disallowed_federation_peer_ip` and `DISALLOWED_FEDERATION_PORTS`, and returns
/// a pinned `reqwest::Client` so DNS rebinding cannot redirect the TCP connection to an internal IP.
async fn build_verified_federation_client(
    base_url: &str,
    timeout_dur: Duration,
    allow_ephemeral_loopback: bool,
) -> Result<reqwest::Client, StatusCode> {
    if !is_safe_federation_peer_base_url_inner(base_url, allow_ephemeral_loopback) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let parsed = reqwest::Url::parse(base_url).map_err(|_| StatusCode::BAD_REQUEST)?;
    let port = parsed
        .port_or_known_default()
        .ok_or(StatusCode::BAD_REQUEST)?;
    if is_disallowed_federation_port(port) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut builder = reqwest::Client::builder()
        .timeout(timeout_dur)
        .redirect(reqwest::redirect::Policy::none());

    let raw_host = parsed.host_str().ok_or(StatusCode::BAD_REQUEST)?;
    let clean_host = raw_host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.');
    if clean_host.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Ok(ip) = clean_host.parse::<IpAddr>() {
        if is_disallowed_federation_peer_ip_inner(ip, allow_ephemeral_loopback) {
            return Err(StatusCode::BAD_REQUEST);
        }
    } else {
        if is_disallowed_federation_hostname(&clean_host.to_ascii_lowercase()) {
            return Err(StatusCode::BAD_REQUEST);
        }
        let resolved: Vec<SocketAddr> = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::lookup_host((clean_host, port)),
        )
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?
        .map_err(|_| StatusCode::BAD_GATEWAY)?
        .collect();
        if resolved.is_empty() {
            return Err(StatusCode::BAD_GATEWAY);
        }
        for addr in &resolved {
            if is_disallowed_federation_port(addr.port())
                || is_disallowed_federation_peer_ip_inner(addr.ip(), allow_ephemeral_loopback)
            {
                return Err(StatusCode::BAD_REQUEST);
            }
        }
        builder = builder.resolve_to_addrs(raw_host, &resolved);
    }

    builder
        .build()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn scrub_string_leaves_deep(val: &mut Value) {
    match val {
        Value::String(s) => {
            let scrubbed_rtsp = scrub_rtsp_credentials(s);
            *s = scrub_yaml_credentials(&scrubbed_rtsp);
        }
        Value::Array(arr) => {
            for item in arr {
                scrub_string_leaves_deep(item);
            }
        }
        Value::Object(map) => {
            for v in map.values_mut() {
                scrub_string_leaves_deep(v);
            }
        }
        _ => {}
    }
}

pub fn scrub_federation_response_bytes(content_type: &str, body_bytes: Vec<u8>) -> Vec<u8> {
    if let Ok(parsed_json) = serde_json::from_slice::<Value>(&body_bytes) {
        let mut scrubbed = scrub_json_value(&parsed_json);
        scrub_string_leaves_deep(&mut scrubbed);
        if let Ok(out) = serde_json::to_vec(&scrubbed) {
            return out;
        }
    }
    let _ = content_type;
    if let Ok(text) = std::str::from_utf8(&body_bytes) {
        let s1 = scrub_rtsp_credentials(text);
        let s2 = scrub_yaml_credentials(&s1);
        return s2.into_bytes();
    }
    body_bytes
}

pub async fn api_federation_nodes(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Response {
    if let Err(err_resp) = authorize_admin_read(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    let allowed = caller_allowed_cameras(&state, &jar, &headers).await;
    let (mut local_cams, peers) = {
        let cfg = state.config.read().await;
        let cams: Vec<String> = cfg.cameras.keys().cloned().collect();
        (cams, cfg.peers.clone())
    };
    if let Some(ref allow_list) = allowed {
        local_cams.retain(|c| allow_list.iter().any(|a| a == c || a == "*"));
    }

    let mut nodes = vec![json!({
        "id": "local",
        "name": "local",
        "primary": true,
        "enabled": true,
        "online": true,
        "version": NINJANVR_VERSION,
        "cameras": local_cams
    })];

    let allow_loopback = state
        .allow_ephemeral_loopback_federation_for_test
        .load(std::sync::atomic::Ordering::Relaxed);

    for (peer_id, peer_cfg) in peers {
        let base_url = peer_cfg.url.trim().trim_end_matches('/').to_string();
        let scrubbed_url = scrub_rtsp_credentials(&base_url);
        let mut online = false;
        let mut version = "unknown".to_string();
        let mut peer_cams: Vec<String> = Vec::new();
        let tok_env_trimmed = peer_cfg.token_env.trim();
        let tok_env_valid =
            tok_env_trimmed.is_empty() || is_allowed_peer_token_env(tok_env_trimmed);
        let hits_active_non_tls_listener =
            ninjanvr_config::extract_http_url_effective_port(&base_url)
                .is_some_and(|p| state.is_active_http_or_metrics_port(p));

        if peer_cfg.enabled
            && tok_env_valid
            && !hits_active_non_tls_listener
            && let Ok(client) =
                build_verified_federation_client(&base_url, Duration::from_secs(3), allow_loopback)
                    .await
        {
            let mut req = client.get(format!("{base_url}/api/stats"));
            if !tok_env_trimmed.is_empty()
                && let Ok(tok) = std::env::var(tok_env_trimmed)
                && !tok.trim().is_empty()
            {
                req = req.header(header::AUTHORIZATION, format!("Bearer {}", tok.trim()));
            }
            if let Ok(resp) = req.send().await
                && resp.status().is_success()
                && let Ok(stats_bytes) = read_upstream_body_bounded(resp, 2 * 1024 * 1024).await
                && let Ok(stats_json) = serde_json::from_slice::<Value>(&stats_bytes)
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
                    if let Some(ref allow_list) = allowed {
                        peer_cams.retain(|c| allow_list.iter().any(|a| a == c || a == "*"));
                    }
                }
            }
        }

        nodes.push(json!({
            "id": peer_id,
            "name": peer_id,
            "primary": false,
            "enabled": peer_cfg.enabled,
            "online": online,
            "url": scrubbed_url,
            "version": version,
            "cameras": peer_cams
        }));
    }

    Json(json!({ "nodes": nodes })).into_response()
}

pub const MAX_PROXY_REQUEST_BODY_BYTES: usize = 1024 * 1024;

fn is_disallowed_federation_upstream_content_type(ct: &str) -> bool {
    let media_type = ct
        .split(';')
        .next()
        .unwrap_or(ct)
        .trim()
        .to_ascii_lowercase();
    media_type.contains("html")
        || media_type.contains("svg")
        || media_type.contains("javascript")
        || media_type.contains("ecmascript")
        || media_type == "text/xml"
        || media_type == "application/xml"
}

pub async fn api_federation_proxy(
    State(state): State<AppState>,
    method: axum::http::Method,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath((node, subpath)): AxumPath<(String, String)>,
    uri: Uri,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > MAX_PROXY_REQUEST_BODY_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "success": false,
                "message": "Federation proxy request body exceeds maximum allowed size"
            })),
        )
            .into_response();
    }
    if method != axum::http::Method::GET
        && method != axum::http::Method::HEAD
        && !verify_cookie_csrf_origin(&headers, &uri)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    if let Err(err_resp) = authorize_admin_read(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    let Some(decoded_sub) = percent_decode_path(&subpath) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Invalid federation proxy subpath"
            })),
        )
            .into_response();
    };
    let decoded_sub_lc = decoded_sub.to_ascii_lowercase();
    if subpath.contains("..")
        || decoded_sub.contains("..")
        || decoded_sub_lc.contains("%2e")
        || decoded_sub_lc.contains("%2f")
        || decoded_sub_lc.contains("%5c")
        || subpath.contains(['\\', '@', '?', '#'])
        || decoded_sub.contains(['\\', '@', '?', '#'])
        || subpath.chars().any(|c| c.is_control() || c.is_whitespace())
        || decoded_sub
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
        || decoded_sub.split('/').any(|seg| seg == "." || seg == "..")
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Invalid federation proxy subpath"
            })),
        )
            .into_response();
    }
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

    let tok_env_trimmed = peer_cfg.token_env.trim();
    if !tok_env_trimmed.is_empty() && !is_allowed_peer_token_env(tok_env_trimmed) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": format!("Federated peer node '{node}' uses a disallowed token_env variable")
            })),
        )
            .into_response();
    }

    let base_url = peer_cfg.url.trim().trim_end_matches('/');
    if ninjanvr_config::extract_http_url_effective_port(base_url)
        .is_some_and(|p| state.is_active_http_or_metrics_port(p))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": format!("Federated peer node '{node}' has an invalid or disallowed base URL")
            })),
        )
            .into_response();
    }
    let allow_loopback = state
        .allow_ephemeral_loopback_federation_for_test
        .load(std::sync::atomic::Ordering::Relaxed);
    let client = match build_verified_federation_client(
        base_url,
        Duration::from_secs(5),
        allow_loopback,
    )
    .await
    {
        Ok(c) => c,
        Err(StatusCode::BAD_REQUEST) => {
            return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "success": false,
                        "message": format!("Federated peer node '{node}' has an invalid or disallowed base URL")
                    })),
                )
                    .into_response();
        }
        Err(status) => {
            return (
                    status,
                    Json(json!({
                        "success": false,
                        "node": node,
                        "message": format!("Federated peer node '{node}' DNS resolution or client setup failed")
                    })),
                )
                    .into_response();
        }
    };

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

    let req_method = match method {
        axum::http::Method::POST => reqwest::Method::POST,
        axum::http::Method::HEAD => reqwest::Method::HEAD,
        _ => reqwest::Method::GET,
    };
    let mut req = client.request(req_method, &target_url);
    if !tok_env_trimmed.is_empty()
        && let Ok(tok) = std::env::var(tok_env_trimmed)
        && !tok.trim().is_empty()
    {
        req = req.header(header::AUTHORIZATION, format!("Bearer {}", tok.trim()));
    }
    if method == axum::http::Method::POST {
        if let Some(ct) = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.chars().any(char::is_control))
        {
            req = req.header(header::CONTENT_TYPE, ct);
        }
        if !body.is_empty() {
            req = req.body(body.to_vec());
        }
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
            if is_disallowed_federation_upstream_content_type(&content_type) {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "success": false,
                        "node": node,
                        "message": "Federated peer returned disallowed executable/HTML Content-Type"
                    })),
                )
                    .into_response();
            }
            let Ok(body_bytes) = read_upstream_body_bounded(upstream, 16 * 1024 * 1024).await
            else {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "success": false,
                        "node": node,
                        "message": "Federated peer response exceeded maximum allowed size"
                    })),
                )
                    .into_response();
            };
            let scrubbed_bytes = scrub_federation_response_bytes(&content_type, body_bytes);
            (
                status,
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                    (
                        header::CONTENT_SECURITY_POLICY,
                        "default-src 'none'; sandbox; frame-ancestors 'none'".to_string(),
                    ),
                    (header::X_FRAME_OPTIONS, "DENY".to_string()),
                    (header::REFERRER_POLICY, "no-referrer".to_string()),
                ],
                scrubbed_bytes,
            )
                .into_response()
        }
        Err(err) => {
            let scrubbed_err = scrub_rtsp_credentials(&err.to_string());
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "success": false,
                    "node": node,
                    "message": format!("Federated peer node '{node}' unreachable: {scrubbed_err}")
                })),
            )
                .into_response()
        }
    }
}

pub async fn api_camera_proxy_root(
    State(state): State<AppState>,
    method: axum::http::Method,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(camera): AxumPath<String>,
    uri: Uri,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > MAX_PROXY_REQUEST_BODY_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "success": false,
                "message": "Camera proxy request body exceeds maximum allowed size"
            })),
        )
            .into_response();
    }
    if method != axum::http::Method::GET
        && method != axum::http::Method::HEAD
        && !verify_cookie_csrf_origin(&headers, &uri)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    if let Err(err_resp) = authorize_admin_read(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    if !caller_can_mutate(&state, &jar, &headers, true).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Admin role required" })),
        )
            .into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    handle_camera_webui_proxy_with_request(
        &state,
        &method,
        &headers,
        &camera,
        "",
        uri.query(),
        body,
    )
    .await
}

pub async fn api_camera_proxy_subpath(
    State(state): State<AppState>,
    method: axum::http::Method,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath((camera, subpath)): AxumPath<(String, String)>,
    uri: Uri,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > MAX_PROXY_REQUEST_BODY_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "success": false,
                "message": "Camera proxy request body exceeds maximum allowed size"
            })),
        )
            .into_response();
    }
    if method != axum::http::Method::GET
        && method != axum::http::Method::HEAD
        && !verify_cookie_csrf_origin(&headers, &uri)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    if let Err(err_resp) = authorize_admin_read(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    if !caller_can_mutate(&state, &jar, &headers, true).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Admin role required" })),
        )
            .into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    handle_camera_webui_proxy_with_request(
        &state,
        &method,
        &headers,
        &camera,
        &subpath,
        uri.query(),
        body,
    )
    .await
}

/// Resolves DNS (or literal IP) for a camera WebUI `base_url`, verifies every resolved
/// `SocketAddr` against `is_disallowed_federation_peer_ip` and internal NinjaNVR / `go2rtc` /
/// OpenMetrics ports, and returns a pinned `reqwest::Client` to prevent DNS rebinding SSRF.
async fn build_verified_camera_webui_client(
    base_url: &str,
    state: &AppState,
    timeout_dur: Duration,
) -> Result<reqwest::Client, StatusCode> {
    let parsed = reqwest::Url::parse(base_url).map_err(|_| StatusCode::BAD_REQUEST)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let port = parsed
        .port_or_known_default()
        .ok_or(StatusCode::BAD_REQUEST)?;
    if parsed.port() == Some(0)
        || ninjanvr_config::is_disallowed_internal_service_port(port)
        || state.is_active_listener_port(port)
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut builder = reqwest::Client::builder()
        .timeout(timeout_dur)
        .redirect(reqwest::redirect::Policy::none());

    let raw_host = parsed.host_str().ok_or(StatusCode::BAD_REQUEST)?;
    let clean_host = raw_host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.');
    if clean_host.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Ok(ip) = clean_host.parse::<IpAddr>() {
        if is_disallowed_federation_peer_ip(ip) {
            return Err(StatusCode::BAD_REQUEST);
        }
    } else {
        if is_disallowed_federation_hostname(&clean_host.to_ascii_lowercase()) {
            return Err(StatusCode::BAD_REQUEST);
        }
        let resolved: Vec<SocketAddr> = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::lookup_host((clean_host, port)),
        )
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?
        .map_err(|_| StatusCode::BAD_GATEWAY)?
        .collect();
        if resolved.is_empty() {
            return Err(StatusCode::BAD_GATEWAY);
        }
        for addr in &resolved {
            if ninjanvr_config::is_disallowed_internal_service_port(addr.port())
                || state.is_active_listener_port(addr.port())
                || is_disallowed_federation_peer_ip(addr.ip())
            {
                return Err(StatusCode::BAD_REQUEST);
            }
        }
        builder = builder.resolve_to_addrs(raw_host, &resolved);
    }

    builder
        .build()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[cfg(test)]
pub(crate) async fn handle_camera_webui_proxy(
    state: &AppState,
    camera: &str,
    subpath: &str,
    query: Option<&str>,
) -> Response {
    let empty_headers = HeaderMap::new();
    handle_camera_webui_proxy_with_request(
        state,
        &axum::http::Method::GET,
        &empty_headers,
        camera,
        subpath,
        query,
        axum::body::Bytes::new(),
    )
    .await
}

pub(crate) async fn handle_camera_webui_proxy_with_request(
    state: &AppState,
    method: &axum::http::Method,
    req_headers: &HeaderMap,
    camera: &str,
    subpath: &str,
    query: Option<&str>,
    req_body: axum::body::Bytes,
) -> Response {
    let Some(decoded_sub) = percent_decode_path(subpath) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Invalid camera proxy subpath"
            })),
        )
            .into_response();
    };
    let decoded_sub_lc = decoded_sub.to_ascii_lowercase();
    if subpath.contains("..")
        || decoded_sub.contains("..")
        || decoded_sub_lc.contains("%2e")
        || decoded_sub_lc.contains("%2f")
        || decoded_sub_lc.contains("%5c")
        || subpath.contains(['\\', '@', '?', '#'])
        || decoded_sub.contains(['\\', '@', '?', '#'])
        || subpath.chars().any(|c| c.is_control() || c.is_whitespace())
        || decoded_sub
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
        || decoded_sub.split('/').any(|seg| seg == "." || seg == "..")
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Invalid camera proxy subpath"
            })),
        )
            .into_response();
    }

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

    let client = match build_verified_camera_webui_client(&base_url, state, Duration::from_secs(4))
        .await
    {
        Ok(c) => c,
        Err(StatusCode::BAD_REQUEST) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "success": false,
                    "message": format!("Camera '{camera}' Web UI URL targets a disallowed address or port")
                })),
            )
                .into_response();
        }
        Err(status) => {
            return (
                status,
                Json(json!({
                    "success": false,
                    "camera": camera,
                    "message": format!("Camera '{camera}' Web UI DNS resolution or client setup failed")
                })),
            )
                .into_response();
        }
    };

    let upstream_method = match *method {
        axum::http::Method::POST => reqwest::Method::POST,
        axum::http::Method::HEAD => reqwest::Method::HEAD,
        _ => reqwest::Method::GET,
    };
    let mut out_req = client.request(upstream_method, &target_url);
    if *method == axum::http::Method::POST {
        if let Some(ct) = req_headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.chars().any(char::is_control))
        {
            out_req = out_req.header(header::CONTENT_TYPE, ct);
        }
        if !req_body.is_empty() {
            out_req = out_req.body(req_body.to_vec());
        }
    }

    match out_req.send().await {
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
                .and_then(|loc| {
                    let trimmed = loc.trim();
                    if trimmed.contains(['\\', '@'])
                        || trimmed.contains("..")
                        || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
                    {
                        return None;
                    }
                    let rel = if let Some(after_base) = trimmed.strip_prefix(&base_url) {
                        if !after_base.is_empty() && !after_base.starts_with(['/', '?', '#']) {
                            return None;
                        }
                        after_base.trim_start_matches('/')
                    } else if (trimmed.starts_with('/') && !trimmed.starts_with("//"))
                        || (!trimmed.contains("://")
                            && !trimmed.starts_with("//")
                            && !trimmed.contains(':'))
                    {
                        trimmed.trim_start_matches('/')
                    } else {
                        return None;
                    };
                    let path_part = rel.split(['?', '#']).next().unwrap_or(rel);
                    let decoded_rel = percent_decode_path(path_part)?;
                    if decoded_rel.contains("..")
                        || decoded_rel.contains(['\\', '@'])
                        || decoded_rel.split('/').any(|seg| seg == "." || seg == "..")
                    {
                        return None;
                    }
                    Some(format!("/api/camera-proxy/{camera}/{rel}"))
                });
            let Ok(body_bytes) = read_upstream_body_bounded(upstream, 16 * 1024 * 1024).await
            else {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "success": false,
                        "camera": camera,
                        "message": "Camera Web UI response exceeded maximum allowed size"
                    })),
                )
                    .into_response();
            };
            // Isolate proxied camera HTML/JS inside an opaque sandboxed origin (`sandbox` without
            // `allow-same-origin` or `allow-scripts`) so untrusted camera firmware can never read
            // NinjaNVR Same-Origin state or issue authenticated `/api/*` requests.
            let mut resp = (
                status,
                [
                    (header::CONTENT_TYPE, content_type),
                    (
                        header::CONTENT_SECURITY_POLICY,
                        "sandbox; frame-ancestors 'self'".to_string(),
                    ),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                    (header::REFERRER_POLICY, "no-referrer".to_string()),
                    (
                        header::HeaderName::from_static("cross-origin-resource-policy"),
                        "same-origin".to_string(),
                    ),
                ],
                body_bytes,
            )
                .into_response();
            if let Some(loc_str) = rewritten_loc
                && let Ok(hv) = header::HeaderValue::from_str(&loc_str)
            {
                resp.headers_mut().insert(header::LOCATION, hv);
            }
            resp
        }
        Err(err) => {
            let scrubbed_base = scrub_rtsp_credentials(&base_url);
            let scrubbed_err = scrub_rtsp_credentials(&err.to_string());
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "success": false,
                    "camera": camera,
                    "target": scrubbed_base,
                    "message": format!("Camera Web UI unreachable: {scrubbed_err}")
                })),
            )
                .into_response()
        }
    }
}
