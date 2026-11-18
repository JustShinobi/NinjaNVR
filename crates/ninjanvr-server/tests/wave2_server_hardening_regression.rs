use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum_extra::extract::cookie::CookieJar;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use ninjanvr_config::CameraConfig;
use ninjanvr_db::{EventDto, NinjaDatabase, RecordingRow, ReviewFilter, ReviewItemDto};
use ninjanvr_server::pipeline::{CameraVisionSession, compute_vision_backoff_delay};
use ninjanvr_server::{
    AccessTarget, JwtClaims, authenticate_read, authorize_mutation, build_router,
    build_router_with_auth, caller_can_mutate, format_signed_oidc_pkce_cookie,
    is_allowed_oidc_client_secret_env, is_allowed_peer_token_env, is_disallowed_federation_peer_ip,
    is_safe_federation_peer_base_url, new_app_state, scrub_federation_response_bytes,
};
use serde_json::{Value, json};
use tower::ServiceExt;

struct TempDirGuard {
    path: PathBuf,
}

impl TempDirGuard {
    fn new(tag: &str) -> Self {
        let nonce: u64 = rand::random();
        let path = std::env::temp_dir().join(format!(
            "ninjanvr-w2-srv-{tag}-{}-{nonce:016x}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn unix_now_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn mint_jwt_with_iat(
    secret: &str,
    sub: &str,
    role: &str,
    allowed_cameras: Option<&str>,
    iat: u64,
) -> String {
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &JwtClaims {
            sub: sub.to_string(),
            role: role.to_string(),
            allowed_cameras: allowed_cameras.map(str::to_string),
            iat,
            exp: iat.saturating_add(3600),
        },
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

fn make_mp4_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let total_len = (8 + payload.len()) as u32;
    let mut out = Vec::with_capacity(total_len as usize);
    out.extend_from_slice(&total_len.to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(payload);
    out
}

fn make_mp4_full_box(fourcc: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut full = Vec::with_capacity(4 + payload.len());
    let vf = ((version as u32) << 24) | (flags & 0x00FF_FFFF);
    full.extend_from_slice(&vf.to_be_bytes());
    full.extend_from_slice(payload);
    make_mp4_box(fourcc, &full)
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
async fn t5_enforced_auth_rejects_internal_ips_cors_logout_csrf_and_per_ip_rate_limits() {
    let dir = TempDirGuard::new("auth-fail-closed");
    let web_dir = dir.path().join("web");
    let media_dir = dir.path().join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(&media_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    let db_path = dir.path().join("ninjanvr.db");
    let cfg_path = dir.path().join("config.yml");
    let yaml = format!(
        r#"
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
  trusted_proxies:
    - "127.0.0.1/32"
cameras:
  front_door:
    enabled: true
version: 0.18-0
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir, false)
        .await
        .unwrap();
    state
        .db
        .update_user_password("admin", "AdminSecret!234", 1000)
        .unwrap();
    state.auth_gate.invalidate_user("admin");

    let app_enforced = build_router_with_auth(state.clone(), true);
    let app_unauth_test = build_router(state.clone());

    // 1. `/api/version` remains 200 OK for container/K8s liveness probes without auth
    let r_ver = app_enforced
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/version")
                .header("x-peer-ip", "127.0.0.1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ver.status(), StatusCode::OK);

    // 2. Unauthenticated internal-IP requests (`192.0.2.10` and loopback `127.0.0.1`) MUST get 401
    for peer_ip in ["192.0.2.10", "127.0.0.1"] {
        for (method, uri) in [
            ("GET", "/api/config"),
            ("GET", "/api/users"),
            ("GET", "/api/events"),
            ("POST", "/api/restart"),
            ("PUT", "/api/config/set"),
        ] {
            let r = app_enforced
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("x-peer-ip", peer_ip)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::UNAUTHORIZED,
                "enforced router must reject unauthenticated {method} {uri} from {peer_ip} with 401"
            );
        }
    }

    // 3. Direct helper calls with `x-ninjanvr-enforce-auth: 1` fail closed (never escalate to admin)
    let mut enforced_hdrs = HeaderMap::new();
    enforced_hdrs.insert("x-ninjanvr-enforce-auth", "1".parse().unwrap());
    enforced_hdrs.insert("x-peer-ip", "127.0.0.1".parse().unwrap());
    let empty_jar = CookieJar::new();
    let test_uri: Uri = "/api/config/set".parse().unwrap();

    assert_eq!(
        authenticate_read(&state, &empty_jar, &enforced_hdrs)
            .await
            .unwrap_err()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        authorize_mutation(
            &state,
            &empty_jar,
            &test_uri,
            &enforced_hdrs,
            true,
            AccessTarget::Global,
        )
        .await
        .unwrap_err()
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(!caller_can_mutate(&state, &empty_jar, &enforced_hdrs, false).await);

    // Meanwhile, `build_router(state)` (`enforce_auth = false`) keeps working for internal tests
    let r_test_router = app_unauth_test
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_test_router.status(), StatusCode::OK);

    // 4. Restrictive CORS does NOT emit wildcard `Access-Control-Allow-Origin: *`
    let r_cors = app_enforced
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/version")
                .header(header::ORIGIN, "https://evil.example.invalid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        r_cors
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
        "default restrictive CORS must not reflect or wildcard Access-Control-Allow-Origin"
    );

    // 5. `GET /logout` and `GET /api/logout` redirect without clearing cookie; `POST /api/logout` clears it
    for get_logout_uri in ["/logout", "/api/logout"] {
        let r_get_logout = app_enforced
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(get_logout_uri)
                    .header(header::COOKIE, "ninjanvr_token=keep_alive_tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_get_logout.status(), StatusCode::SEE_OTHER);
        assert!(
            r_get_logout.headers().get(header::SET_COOKIE).is_none(),
            "GET {get_logout_uri} must not clear session cookie (prevents <img src='/logout'> CSRF)"
        );
    }

    let r_post_logout = app_enforced
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/logout")
                .header(header::HOST, "nvr.example.invalid")
                .header(header::ORIGIN, "https://nvr.example.invalid")
                .header(header::COOKIE, "ninjanvr_token=keep_alive_tok")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_post_logout.status(), StatusCode::SEE_OTHER);
    assert!(
        r_post_logout
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s.contains("ninjanvr_token=")),
        "POST /api/logout with valid Origin must clear session cookie"
    );

    // 6. Per-IP login rate limit isolates legitimate operator IP from attacker IP + enforces Secure cookie over HTTPS
    for _ in 0..8 {
        let r_bad = app_enforced
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/login")
                    .header("x-peer-ip", "198.51.100.10")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"user":"admin","password":"wrong"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_bad.status(), StatusCode::UNAUTHORIZED);
    }
    let r_attacker_locked = app_enforced
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/login")
                .header("x-peer-ip", "198.51.100.10")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"user":"admin","password":"AdminSecret!234"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_attacker_locked.status(), StatusCode::TOO_MANY_REQUESTS);

    // Legitimate operator from `127.0.0.1` with trusted `X-Forwarded-Proto: https` logs in and gets `Secure` cookie
    let r_legit_ok = app_enforced
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/login")
                .header("x-peer-ip", "127.0.0.1")
                .header("x-forwarded-proto", "https")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"user":"admin","password":"AdminSecret!234"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_legit_ok.status(), StatusCode::OK);
    let login_cookie = r_legit_ok
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert!(
        login_cookie.contains("Secure"),
        "HTTPS login via trusted X-Forwarded-Proto must set Secure attribute on session cookie: {login_cookie}"
    );
}

#[tokio::test]
async fn t5_password_changed_at_revokes_jwt_cookies_and_active_ws_immediately() {
    let dir = TempDirGuard::new("jwt-revocation");
    let web_dir = dir.path().join("web");
    let media_dir = dir.path().join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(&media_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    let db_path = dir.path().join("ninjanvr.db");
    let cfg_path = dir.path().join("config.yml");
    let yaml = format!(
        r#"
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
cameras:
  front_door:
    enabled: true
version: 0.18-0
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir, false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("alice", "OldPass!1234", "operator", "front_door", 1000)
        .unwrap();

    let created_epoch = state
        .db
        .get_user_password_changed_at_epoch("alice")
        .unwrap()
        .unwrap() as u64;
    let alice_jwt = mint_jwt_with_iat(
        &state.jwt_secret,
        "alice",
        "operator",
        Some("front_door"),
        created_epoch,
    );
    let alice_cookie = format!("ninjanvr_token={alice_jwt}");
    let admin_bearer = format!(
        "Bearer {}",
        mint_jwt_with_iat(
            &state.jwt_secret,
            "admin",
            "admin",
            Some("*"),
            unix_now_u64() + 5,
        )
    );

    let app = build_router_with_auth(state.clone(), true);

    // 1. Pre-rotation JWT cookie succeeds and populates AuthSessionGate in-memory cache
    let r_before = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/profile")
                .header(header::COOKIE, &alice_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_before.status(), StatusCode::OK);
    assert!(
        state
            .auth_gate
            .try_get_cached_user(state.db.path(), "alice")
            .is_some(),
        "AuthSessionGate must cache DbUser in RAM after initial lookup"
    );

    // 2. Open a live `/ws` stream using the pre-rotation JWT
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = ws_listener.local_addr().unwrap();
    let app_for_ws = app.clone();
    let ws_srv = tokio::spawn(async move {
        let _ = axum::serve(ws_listener, app_for_ws).await;
    });

    let mut ws_req =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(format!(
            "ws://{ws_addr}/ws"
        ))
        .unwrap();
    ws_req.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {alice_jwt}").parse().unwrap(),
    );
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(ws_req).await.unwrap();

    // Wait 1.1s so the new password_changed_at second is strictly greater than `created_epoch`
    tokio::time::sleep(Duration::from_millis(1100)).await;

    // 3. Rotate alice's password via `PUT /api/users/alice/password`
    let r_pw_change = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/users/alice/password")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"password":"NewRotatedPassword!999"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_pw_change.status(), StatusCode::OK);

    // 4. Pre-existing JWT cookie is IMMEDIATELY rejected with 401 Unauthorized!
    let r_after = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/profile")
                .header(header::COOKIE, &alice_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_after.status(),
        StatusCode::UNAUTHORIZED,
        "JWT cookie issued prior to password_changed_at must be immediately rejected with 401"
    );

    // 5. Active `/ws` stream is terminated when a frame is sent/received!
    let _ = ws_stream
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"topic":"onConnect"}"#.into(),
        ))
        .await;
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(msg_res) = ws_stream.next().await {
            match msg_res {
                Ok(tokio_tungstenite::tungstenite::Message::Close(_)) | Err(_) => return true,
                _ => {}
            }
        }
        true
    })
    .await
    .unwrap_or(false);
    assert!(
        closed,
        "Active /ws connection authenticated with pre-rotation JWT must close upon password reset"
    );

    ws_srv.abort();
}

#[tokio::test]
async fn t5_oidc_rs256_fails_closed_on_discovery_404_and_federation_blocks_ssrf_and_jwt_secret_env()
{
    let dir = TempDirGuard::new("oidc-fed-ssrf");
    let web_dir = dir.path().join("web");
    let media_dir = dir.path().join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(&media_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    // Mock OIDC IdP whose `/.well-known/openid-configuration` returns 404 while `/oauth/v2/token`
    // returns an unverified asymmetric RS256 `id_token` (or bare `access_token`).
    let idp_mode = Arc::new(std::sync::Mutex::new("rs256_disc_404".to_string()));
    let idp_mode_tok = Arc::clone(&idp_mode);
    let idp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let idp_addr = idp_listener.local_addr().unwrap();
    let issuer = format!("http://{idp_addr}");
    let iss_for_tok = issuer.clone();

    let mock_idp = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(|| async { StatusCode::NOT_FOUND }),
        )
        .route(
            "/oauth/v2/token",
            post(move || {
                let mode = idp_mode_tok.lock().unwrap().clone();
                let iss = iss_for_tok.clone();
                let exp = unix_now_u64() + 3600;
                async move {
                    if mode == "bare_access_token" {
                        return Json(json!({ "access_token": "bare_tok" })).into_response();
                    }
                    let h = base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .encode(r#"{"alg":"RS256","typ":"JWT"}"#);
                    let p = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                        json!({
                            "sub": "attacker",
                            "preferred_username": "admin",
                            "iss": iss,
                            "aud": "nvr-client",
                            "exp": exp,
                            "groups": ["admins"]
                        })
                        .to_string()
                        .as_bytes(),
                    );
                    Json(json!({
                        "id_token": format!("{h}.{p}.unverified_forged_rs256_signature")
                    }))
                    .into_response()
                }
            }),
        );
    let idp_srv = tokio::spawn(async move {
        let _ = axum::serve(idp_listener, mock_idp).await;
    });

    let db_path = dir.path().join("ninjanvr.db");
    let cfg_path = dir.path().join("config.yml");
    let yaml = format!(
        r#"
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
  oidc:
    enabled: true
    issuer_url: "{}"
    client_id: "nvr-client"
cameras:
  front_door:
    enabled: true
version: 0.18-0
"#,
        db_path.display(),
        issuer
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir, false)
        .await
        .unwrap();
    let app = build_router_with_auth(state.clone(), true);
    let admin_bearer = format!(
        "Bearer {}",
        mint_jwt_with_iat(
            &state.jwt_secret,
            "admin",
            "admin",
            Some("*"),
            unix_now_u64() + 2,
        )
    );

    // 1. Unsigned or forged-HMAC `ninjanvr_oidc_pkce` cookie (cookie-tossing CSRF) fails closed with 403
    for forged_cookie in [
        "ninjanvr_oidc_pkce=st1:ver1:http://localhost:5000/api/auth/oidc/callback".to_string(),
        format!(
            "ninjanvr_oidc_pkce={}",
            format_signed_oidc_pkce_cookie(
                "wrong-jwt-secret-key-000000000000",
                "st1",
                "ver1",
                "http://localhost:5000/api/auth/oidc/callback",
            )
        ),
    ] {
        let r_forged = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/callback?code=c1&state=st1")
                    .header(header::COOKIE, forged_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_forged.status(),
            StatusCode::FORBIDDEN,
            "Unsigned or forged-HMAC PKCE cookie must fail closed with 403"
        );
    }

    // 2. Validly signed PKCE cookie + RS256 `id_token` when discovery returns 404 MUST fail closed with 401!
    let valid_pkce_cookie = format!(
        "ninjanvr_oidc_pkce={}",
        format_signed_oidc_pkce_cookie(
            &state.jwt_secret,
            "st1",
            "ver1",
            "http://localhost:5000/api/auth/oidc/callback",
        )
    );
    let r_rs256_404 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/oidc/callback?code=c1&state=st1")
                .header(header::COOKIE, &valid_pkce_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_rs256_404.status(),
        StatusCode::UNAUTHORIZED,
        "Asymmetric RS256 id_token without discovered jwks_uri (404) must fail closed with 401"
    );

    // 3. Bare `access_token` without `id_token` fails closed with 401
    *idp_mode.lock().unwrap() = "bare_access_token".to_string();
    let r_no_id_tok = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/oidc/callback?code=c1&state=st1")
                .header(header::COOKIE, &valid_pkce_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_no_id_tok.status(), StatusCode::UNAUTHORIZED);

    // 4. SSRF & Env Exfiltration guards on `api_federation_proxy` and `client_secret_env` / `token_env`
    assert!(is_disallowed_federation_peer_ip(
        "127.0.0.1".parse().unwrap()
    ));
    assert!(is_disallowed_federation_peer_ip("::1".parse().unwrap()));
    assert!(is_disallowed_federation_peer_ip(
        "64:ff9b::7f00:1".parse().unwrap()
    ));
    assert!(is_disallowed_federation_peer_ip(
        "2002:7f00:1::".parse().unwrap()
    ));
    assert!(is_disallowed_federation_peer_ip(
        "2001:0000:4136:e378:8000:63bf:3fff:fdd2".parse().unwrap()
    ));
    assert!(!is_safe_federation_peer_base_url("http://127.0.0.1:1984"));
    assert!(!is_safe_federation_peer_base_url("http://localhost:5000"));
    assert!(!is_safe_federation_peer_base_url(
        "https://peer.example.invalid:1984"
    ));
    assert!(!is_safe_federation_peer_base_url(
        "https://go2rtc.default.svc.cluster.local:8971"
    ));
    assert!(is_safe_federation_peer_base_url(
        "https://peer.example.invalid:8971"
    ));

    assert!(is_allowed_peer_token_env("NINJANVR_PEER_EAST_TOKEN"));
    assert!(!is_allowed_peer_token_env("NINJANVR_JWT_SECRET"));
    assert!(!is_allowed_peer_token_env("AWS_SECRET_ACCESS_KEY"));
    assert!(!is_allowed_peer_token_env("PATH"));
    assert!(is_allowed_oidc_client_secret_env(
        "NINJANVR_OIDC_CLIENT_SECRET"
    ));
    assert!(!is_allowed_oidc_client_secret_env("NINJANVR_JWT_SECRET"));

    {
        let mut cfg = state.config.write().await;
        cfg.peers.insert(
            "go2rtc_ssrf".to_string(),
            ninjanvr_config::PeerNodeConfig {
                enabled: true,
                url: "http://127.0.0.1:1984".to_string(),
                token_env: String::new(),
            },
        );
        cfg.peers.insert(
            "jwt_exfil".to_string(),
            ninjanvr_config::PeerNodeConfig {
                enabled: true,
                url: "https://peer.example.invalid:8971".to_string(),
                token_env: "NINJANVR_JWT_SECRET".to_string(),
            },
        );
    }

    for blocked_peer in ["go2rtc_ssrf", "jwt_exfil"] {
        let r_fed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/federation/{blocked_peer}/streams"))
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_fed.status(),
            StatusCode::BAD_REQUEST,
            "api_federation_proxy must reject peer '{blocked_peer}' with 400 BAD_REQUEST"
        );
    }

    // 5. Federation response credential scrubber strips RTSP credentials and secret keys
    let raw_upstream = br#"{"streams":{"front_door":["rtsp://cam_user:SuperSecretCamPw@192.0.2.55:554/main"]},"mqtt":{"password":"MqttSecretPassword99"}}"#;
    let scrubbed = scrub_federation_response_bytes("application/json", raw_upstream.to_vec());
    let scrubbed_text = String::from_utf8(scrubbed).unwrap();
    assert!(!scrubbed_text.contains("SuperSecretCamPw"));
    assert!(!scrubbed_text.contains("MqttSecretPassword99"));

    idp_srv.abort();
}

#[tokio::test]
async fn t5_scoped_config_redacts_plates_peers_floorplan_and_internal_topology_for_restricted_viewer()
 {
    let dir = TempDirGuard::new("scoped-config");
    let web_dir = dir.path().join("web");
    let media_dir = dir.path().join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(&media_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    let db_path = dir.path().join("ninjanvr.db");
    let cfg_path = dir.path().join("config.yml");
    let yaml = format!(
        r#"
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
  trusted_proxies:
    - "198.51.100.0/24"
  oidc:
    enabled: true
    issuer_url: "https://idp.example.invalid"
    client_id: "nvr-client"
    client_secret_env: "NINJANVR_OIDC_CLIENT_SECRET"
mqtt:
  enabled: true
  host: "mqtt.example.invalid"
  user: "mqtt_operator"
lpr:
  enabled: true
  known_plates:
    BRA2E19: "Executive Car"
peers:
  branch_a:
    enabled: true
    url: "https://peer-a.example.invalid:8971"
    token_env: "NINJANVR_PEER_A_TOKEN"
ui:
  floorplan:
    rooms:
      - id: "r1"
        name: "Lobby"
        camera: "front_door"
cameras:
  front_door:
    enabled: true
    webui_url: "http://192.0.2.25:8080"
    onvif:
      host: "192.0.2.25"
      port: 2020
      user: "onvif_admin"
    ffmpeg:
      inputs:
        - path: "rtsp://cam_op:cam_pw@192.0.2.25:554/stream1"
          roles: [detect, record]
  vault_cam:
    enabled: true
version: 0.18-0
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir, false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("fd_viewer", "ViewerPass!123", "viewer", "front_door", 1000)
        .unwrap();

    let viewer_bearer = format!(
        "Bearer {}",
        mint_jwt_with_iat(
            &state.jwt_secret,
            "fd_viewer",
            "viewer",
            Some("front_door"),
            unix_now_u64() + 1,
        )
    );
    let admin_bearer = format!(
        "Bearer {}",
        mint_jwt_with_iat(
            &state.jwt_secret,
            "admin",
            "admin",
            Some("*"),
            unix_now_u64() + 1,
        )
    );
    let app = build_router_with_auth(state, true);

    let r_viewer_cfg = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .header(header::AUTHORIZATION, &viewer_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_viewer_cfg.status(), StatusCode::OK);
    let viewer_bytes = to_bytes(r_viewer_cfg.into_body(), 131_072).await.unwrap();
    let viewer_str = std::str::from_utf8(&viewer_bytes).unwrap();
    let viewer_json: Value = serde_json::from_slice(&viewer_bytes).unwrap();

    assert_eq!(viewer_json["lpr"]["known_plates"], json!({}));
    assert_eq!(viewer_json["peers"], json!({}));
    assert_eq!(viewer_json["database"], json!({}));
    assert!(viewer_json["ui"].get("floorplan").is_none());
    assert!(viewer_json["cameras"].get("vault_cam").is_none());
    assert!(viewer_json["cameras"].get("front_door").is_some());
    assert!(!viewer_str.contains("BRA2E19"));
    assert!(!viewer_str.contains("Executive Car"));
    assert!(!viewer_str.contains("peer-a.example.invalid"));
    assert!(!viewer_str.contains("mqtt.example.invalid"));
    assert!(!viewer_str.contains("192.0.2.25"));
    assert!(!viewer_str.contains("onvif_admin"));
    assert!(!viewer_str.contains("NINJANVR_OIDC_CLIENT_SECRET"));

    // Unrestricted admin still receives known_plates, peers, and floorplan
    let r_admin_cfg = app
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_admin_cfg.status(), StatusCode::OK);
    let admin_json: Value =
        serde_json::from_slice(&to_bytes(r_admin_cfg.into_body(), 131_072).await.unwrap()).unwrap();
    assert_eq!(
        admin_json["lpr"]["known_plates"]["BRA2E19"],
        json!("Executive Car")
    );
    assert!(admin_json["peers"].get("branch_a").is_some());
    assert!(admin_json["ui"].get("floorplan").is_some());
}

#[tokio::test]
async fn t5_startup_orphan_sweep_tamper_end_time_closure_and_dynamic_supervision_work() {
    let dir = TempDirGuard::new("tamper-supervision");
    let web_dir = dir.path().join("web");
    let media_dir = dir.path().join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(&media_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    let db_path = dir.path().join("ninjanvr.db");
    let cfg_path = dir.path().join("config.yml");

    // 1. Pre-seed orphaned open `event` and `reviewsegment` rows (`end_time = None`) prior to startup
    {
        let pre_db = NinjaDatabase::open(&db_path).unwrap();
        pre_db
            .upsert_event(&EventDto {
                id: "orphan_ev_1".to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 1000.0,
                end_time: None,
                has_clip: false,
                has_snapshot: false,
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
        pre_db
            .insert_review_segment(&ReviewItemDto {
                id: "orphan_rev_1".to_string(),
                camera: "front_door".to_string(),
                start_time: 1000.0,
                end_time: None,
                severity: "alert".to_string(),
                thumb_path: "/api/front_door/latest.jpg".to_string(),
                data: json!({ "objects": ["camera_tamper"], "detections": [] }),
                has_been_reviewed: false,
            })
            .unwrap();
    }

    let yaml = format!(
        r#"
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
cameras:
  front_door:
    enabled: true
    detect:
      width: 320
      height: 180
      fps: 5
version: 0.18-0
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir, false)
        .await
        .unwrap();

    // Verify startup orphan sweep closed both `orphan_ev_1` and `orphan_rev_1`
    let healed_ev = state.db.get_event("orphan_ev_1").unwrap().unwrap();
    assert_eq!(healed_ev["end_time"].as_f64(), Some(1015.0));
    let healed_rev = state.db.get_review_by_id("orphan_rev_1").unwrap().unwrap();
    assert_eq!(healed_rev.end_time, Some(1015.0));

    // 2. Verify exponential vision backoff schedule (5s -> 10s -> 20s -> 40s -> 60s)
    assert_eq!(compute_vision_backoff_delay(0), Duration::from_secs(5));
    assert_eq!(compute_vision_backoff_delay(1), Duration::from_secs(10));
    assert_eq!(compute_vision_backoff_delay(2), Duration::from_secs(20));
    assert_eq!(compute_vision_backoff_delay(3), Duration::from_secs(40));
    assert_eq!(compute_vision_backoff_delay(4), Duration::from_secs(60));
    assert_eq!(compute_vision_backoff_delay(9), Duration::from_secs(60));

    // 3. `camera_tamper` review segment gets `end_time` populated when normal frames resume AND on stream disconnect!
    let mut cam_cfg = CameraConfig::default();
    cam_cfg.detect.width = 320;
    cam_cfg.detect.height = 180;
    cam_cfg.detect.fps = 3;
    let mut session =
        CameraVisionSession::new("front_door", cam_cfg, state.detector.model_dimensions());
    session.tamper_threshold_frames = 3;

    let y_len = (320 * 180) as usize;
    let uv_len = y_len / 4;
    let blind_frame = Arc::new(vec![10u8; y_len + uv_len * 2]);
    let mut normal_vec = vec![128u8; y_len + uv_len * 2];
    for (i, px) in normal_vec.iter_mut().enumerate().take(y_len) {
        *px = ((i * 23) % 120 + 30) as u8;
    }
    let normal_frame = Arc::new(normal_vec);

    for step_idx in 1..=3u64 {
        session
            .step_frame(
                &state.pipeline,
                Arc::clone(&blind_frame),
                2000.0 + step_idx as f64,
                step_idx,
            )
            .await;
    }
    let tamper_rev_id = session
        .active_tamper_review
        .as_ref()
        .expect("active_tamper_review must be populated on tamper trigger")
        .id
        .clone();
    assert!(
        state
            .db
            .get_review_by_id(&tamper_rev_id)
            .unwrap()
            .unwrap()
            .end_time
            .is_none(),
        "open tamper review must initially have end_time = None"
    );

    // Resume normal frame at t = 2010.0 -> closes `tamper_rev_id` with `end_time = Some(2010.0)`
    session
        .step_frame(&state.pipeline, Arc::clone(&normal_frame), 2010.0, 4)
        .await;
    assert!(session.active_tamper_review.is_none());
    let closed_on_recovery = state.db.get_review_by_id(&tamper_rev_id).unwrap().unwrap();
    assert_eq!(closed_on_recovery.end_time, Some(2010.0));

    // Trigger tamper a second time, then simulate stream disconnect -> closes with `end_time = Some(2023.0)`
    for step_idx in 1..=3u64 {
        session
            .step_frame(
                &state.pipeline,
                Arc::clone(&blind_frame),
                2020.0 + step_idx as f64,
                4 + step_idx,
            )
            .await;
    }
    let tamper_disc_id = session.active_tamper_review.as_ref().unwrap().id.clone();
    session.handle_stream_disconnected(&state.pipeline).await;
    let closed_on_disc = state.db.get_review_by_id(&tamper_disc_id).unwrap().unwrap();
    assert_eq!(closed_on_disc.end_time, Some(2023.0));
    assert!(
        state
            .db
            .query_reviews(&ReviewFilter::default())
            .unwrap()
            .into_iter()
            .all(|r| r.end_time.is_some()),
        "zero review segments may remain stuck with end_time = None"
    );

    // 4. `PUT /api/config/set` & `POST /api/restart` apply structured patch, reload go2rtc, and broadcast WS `"config"`
    let mut ws_rx = state.comms.subscribe_ws();
    let initial_reloads = state.go2rtc_supervisor.reload_count();
    let admin_bearer = format!(
        "Bearer {}",
        mint_jwt_with_iat(
            &state.jwt_secret,
            "admin",
            "admin",
            Some("*"),
            unix_now_u64() + 1,
        )
    );
    let app = build_router_with_auth(state.clone(), true);

    let r_set = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/set")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"cameras":{"side_alley":{"enabled":true,"detect":{"width":640,"height":360,"fps":5}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_set.status(), StatusCode::OK);
    assert!(state.config.read().await.cameras.contains_key("side_alley"));
    assert!(state.go2rtc_supervisor.reload_count() > initial_reloads);

    let reloads_after_set = state.go2rtc_supervisor.reload_count();
    let r_restart = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/restart")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_restart.status(), StatusCode::OK);
    assert!(state.go2rtc_supervisor.reload_count() > reloads_after_set);

    let mut saw_config_ws = false;
    while let Ok(msg) = ws_rx.try_recv() {
        if let Ok(val) = serde_json::from_str::<Value>(&msg)
            && val["topic"] == "config"
        {
            saw_config_ws = true;
        }
    }
    assert!(
        saw_config_ws,
        "ConfigCoordinator commits and POST /api/restart must broadcast 'config' topic on /ws"
    );
}

#[tokio::test]
async fn t5_head_latest_jpg_fast_path_vod_window_cache_and_mcp_spec_compliance_work() {
    let dir = TempDirGuard::new("head-vod-mcp");
    let web_dir = dir.path().join("web");
    let media_dir = dir.path().join("media");
    let rec_dir = media_dir.join("recordings/2026-11-17/12/front_door");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(&rec_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    let db_path = dir.path().join("ninjanvr.db");
    let cfg_path = dir.path().join("config.yml");
    let yaml = format!(
        r#"
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
cameras:
  front_door:
    enabled: true
version: 0.18-0
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir, false)
        .await
        .unwrap();
    let admin_bearer = format!(
        "Bearer {}",
        mint_jwt_with_iat(
            &state.jwt_secret,
            "admin",
            "admin",
            Some("*"),
            unix_now_u64() + 1,
        )
    );

    // 1. Seed in-memory `camera_metrics` ONLY (zero YUV/JPEG frames on disk) and verify fast-path `HEAD`
    state.camera_metrics.write().await.insert(
        "front_door".to_string(),
        ninjanvr_server::pipeline::CameraRuntimeMetrics {
            camera_fps: 15.0,
            process_fps: 14.0,
            skipped_fps: 1.0,
            detection_fps: 4.5,
            total_inferences: 42,
            pid: 999,
            last_frame_ts: 1_763_000_777.0,
            night_mode: false,
        },
    );

    let app = build_router_with_auth(state.clone(), true);
    let r_head = app
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri("/api/front_door/latest.jpg")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_head.status(), StatusCode::OK);
    assert_eq!(
        r_head
            .headers()
            .get("x-last-frame-timestamp")
            .and_then(|v| v.to_str().ok()),
        Some("1763000777")
    );
    let head_body = to_bytes(r_head.into_body(), 1024).await.unwrap();
    assert!(
        head_body.is_empty(),
        "HEAD /api/{{cam}}/latest.jpg must return empty body without JPEG encoding"
    );
    assert!(
        state.lazy_snapshot_cache.read().await.is_empty(),
        "HEAD /api/{{cam}}/latest.jpg must not populate lazy_snapshot_cache"
    );

    // Verify Prometheus `/api/metrics` exports monotonic `ninjanvr_detector_inferences_total`
    let r_prom = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/metrics")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_prom.status(), StatusCode::OK);
    let prom_str =
        String::from_utf8(to_bytes(r_prom.into_body(), 65536).await.unwrap().to_vec()).unwrap();
    assert!(
        prom_str.contains("ninjanvr_detector_inferences_total{camera=\"front_door\"} 42\n"),
        "Prometheus exposition must include ninjanvr_detector_inferences_total: {prom_str}"
    );

    // 2. `VodWindowCache`: `vod_index_m3u8` populates cache once, and `vod_init_mp4` + `vod_segment_m4s` hit cache in O(1)
    let seg_file = rec_dir.join("00.00.mp4");
    std::fs::write(&seg_file, build_synthetic_mp4_bytes()).unwrap();
    state
        .db
        .insert_recording(&RecordingRow {
            id: "w2_vod_seg_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-17/12/front_door/00.00.mp4".to_string(),
            start_time: 5000.0,
            end_time: 5010.0,
            duration: 10.0,
            motion: 10,
            objects: 1,
            dbfs: -25,
            segment_size: 0.2,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

    assert_eq!(state.vod_cache.hit_count(), 0);
    let r_vod_idx = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/vod/front_door/start/5000/end/5020/index.m3u8")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_vod_idx.status(), StatusCode::OK);

    let r_vod_init = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/vod/front_door/start/5000/end/5020/init-v1-a1.mp4")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_vod_init.status(), StatusCode::OK);
    assert_eq!(state.vod_cache.hit_count(), 1);

    let r_vod_m4s = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/vod/front_door/start/5000/end/5020/seg-1-v1-a1.m4s")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_vod_m4s.status(), StatusCode::OK);
    assert_eq!(
        state.vod_cache.hit_count(),
        2,
        "vod_segment_m4s must resolve segment via VodWindowCache hit without re-querying SQLite"
    );

    // 3. `/api/mcp` HTTP & JSON-RPC 2.0 compliance (`GET` SSE/JSON, `resources/list`, `prompts/list`, `200` on `-32601`)
    let r_mcp_sse = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/mcp")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::ACCEPT, "text/event-stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_mcp_sse.status(), StatusCode::OK);
    assert!(
        r_mcp_sse
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/event-stream"))
    );

    let r_mcp_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mcp")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":10,"method":"resources/list","params":{}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_mcp_res.status(), StatusCode::OK);
    let mcp_res_json: Value =
        serde_json::from_slice(&to_bytes(r_mcp_res.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(mcp_res_json["result"]["resources"], json!([]));

    let r_mcp_prompts = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mcp")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":11,"method":"prompts/list","params":{}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_mcp_prompts.status(), StatusCode::OK);
    let mcp_prompts_json: Value =
        serde_json::from_slice(&to_bytes(r_mcp_prompts.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(mcp_prompts_json["result"]["prompts"], json!([]));

    let r_mcp_unknown = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mcp")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":12,"method":"nonexistent/method","params":{}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_mcp_unknown.status(),
        StatusCode::OK,
        "Unknown JSON-RPC method on /api/mcp must return HTTP 200 OK with -32601 JSON-RPC error"
    );
    let mcp_unk_json: Value =
        serde_json::from_slice(&to_bytes(r_mcp_unknown.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(mcp_unk_json["error"]["code"], json!(-32601));
}

#[cfg(unix)]
#[tokio::test]
async fn t5_dynamic_vision_pipeline_supervision_spawns_reconciles_and_shuts_down_workers_cleanly() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDirGuard::new("vision-supervise");
    let media_dir = dir.path().join("media");
    std::fs::create_dir_all(&media_dir).unwrap();
    let db = NinjaDatabase::open(dir.path().join("ninjanvr.db")).unwrap();

    // Fake ffmpeg script: outputs two 160x120 YUV420p frames (28,800 bytes each), then sleeps until SIGTERM
    let fake_ffmpeg = dir.path().join("fake-ffmpeg.sh");
    let script = r#"#!/bin/sh
trap 'exit 0' TERM INT
dd if=/dev/zero bs=28800 count=2 2>/dev/null
sleep 10
"#;
    std::fs::write(&fake_ffmpeg, script).unwrap();
    std::fs::set_permissions(&fake_ffmpeg, std::fs::Permissions::from_mode(0o755)).unwrap();

    let yaml = r#"
cameras:
  front_door:
    enabled: true
    detect:
      enabled: true
      width: 160
      height: 120
      fps: 5
    ffmpeg:
      inputs:
        - path: "rtsp://127.0.0.1:8554/front_door_sub"
          roles: [detect]
version: 0.18-0
"#;
    let cfg = ninjanvr_config::NinjaConfig::from_yaml_str(yaml, dir.path().join("config.yml"))
        .expect("parse vision test config");
    let shared_cfg = Arc::new(tokio::sync::RwLock::new(cfg.clone()));
    let comms = Arc::new(ninjanvr_comms::CommsHub::new());
    comms.init_from_config(&cfg);
    let detector = Arc::new(ninjanvr_infer::AiDetectorEngine::new(&cfg));
    let latest_yuv = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let cam_metrics = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));

    let pipeline = Arc::new(
        ninjanvr_server::pipeline::VisionPipeline::new(
            Arc::clone(&shared_cfg),
            db,
            Arc::clone(&comms),
            detector,
            ninjanvr_media::new_activity_ledger(),
            Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            Arc::clone(&latest_yuv),
            ninjanvr_server::preview_frames::PreviewFrameStore::new(&media_dir),
            Arc::clone(&cam_metrics),
            media_dir,
            ninjanvr_server::LogBufferHub::new(),
            ninjanvr_server::CameraDriverRuntime::new(),
        )
        .with_ffmpeg_bin(fake_ffmpeg),
    );

    Arc::clone(&pipeline).start().await;
    assert_eq!(
        pipeline.active_vision_cameras().await,
        vec!["front_door".to_string()]
    );

    // Wait briefly for the decoupled stdout watch reader to publish the latest YUV frame
    for _ in 0..40 {
        if latest_yuv.read().await.contains_key("front_door") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        latest_yuv.read().await.contains_key("front_door"),
        "Supervised camera vision worker must publish latest YuvFrame via decoupled watch reader"
    );

    // Reconcile config: disable `front_door`, add `side_gate`
    let next_yaml = r#"
cameras:
  front_door:
    enabled: false
  side_gate:
    enabled: true
    detect:
      enabled: true
      width: 160
      height: 120
      fps: 5
    ffmpeg:
      inputs:
        - path: "rtsp://127.0.0.1:8554/side_gate_sub"
          roles: [detect]
version: 0.18-0
"#;
    let next_cfg =
        ninjanvr_config::NinjaConfig::from_yaml_str(next_yaml, dir.path().join("config.yml"))
            .unwrap();
    comms.init_from_config(&next_cfg);
    *shared_cfg.write().await = next_cfg;

    pipeline.reconcile().await;
    assert_eq!(
        pipeline.active_vision_cameras().await,
        vec!["side_gate".to_string()],
        "VisionPipeline::reconcile must stop disabled camera and spawn newly added camera"
    );

    pipeline.shutdown().await;
    assert!(
        pipeline.active_vision_cameras().await.is_empty(),
        "VisionPipeline::shutdown must stop all supervised camera workers cleanly"
    );
}
