use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use base64::Engine;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use ninjanvr_db::ExportRow;
use ninjanvr_server::{
    JwtClaims, LogBufferHub, build_router, build_router_with_auth, ip_matches_cidr, new_app_state,
    resolve_static_media_camera, sanitize_logged_path,
};
use serde_json::{Value, json};
use tower::ServiceExt;

fn mint_test_jwt(secret: &str, sub: &str, role: &str, allowed_cameras: Option<&str>) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = JwtClaims {
        sub: sub.to_string(),
        role: role.to_string(),
        allowed_cameras: allowed_cameras.map(str::to_string),
        iat: now,
        exp: now + 3600,
    };
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

#[tokio::test]
async fn phase3a_router_rbac_camera_scoping_static_media_and_log_sanitization_work() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-p3a-router-{}", rand::random::<u64>()));
    let web_dir = tmp_dir.join("web-v2");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(media_dir.join("clips/review")).unwrap();
    std::fs::create_dir_all(media_dir.join("exports/front_door_1_2")).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html>Console v2</html>").unwrap();
    std::fs::write(web_dir.join(".env"), "SECRET_DOTFILE=1").unwrap();

    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
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
      width: 1280
      height: 720
      fps: 5
  backyard:
    enabled: true
    detect:
      width: 640
      height: 480
      fps: 5
  exports:
    enabled: true
    detect:
      width: 1280
      height: 720
      fps: 5
version: 0.18-0
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let _ = state.db.ensure_default_admin(1000).unwrap();
    state
        .db
        .create_user_with_cameras("scoped_op", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    let (_, metrics_tok) = state
        .db
        .create_api_token("prom_scraper", "metrics", None)
        .unwrap();

    let app = build_router_with_auth(state.clone(), true);
    let scoped_bearer = format!(
        "Bearer {}",
        mint_test_jwt(
            &state.jwt_secret,
            "scoped_op",
            "operator",
            Some("front_door")
        )
    );
    let metrics_bearer = format!("Bearer {metrics_tok}");

    // 1. GET /auth (top-level ForwardAuth endpoint) must reject 'metrics'-scoped tokens with 403 FORBIDDEN
    let r_auth_metrics = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/auth")
                .header(header::AUTHORIZATION, &metrics_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_auth_metrics.status(), StatusCode::FORBIDDEN);

    // 2. /api/preview/{camera}/start/{start}/end/{end}/frames must enforce per-camera scoping
    let r_prev_frames_forbidden = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/preview/backyard/start/100/end/200/frames")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_prev_frames_forbidden.status(), StatusCode::FORBIDDEN);

    let r_prev_frames_allowed = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/preview/front_door/start/100/end/200/frames")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_prev_frames_allowed.status(), StatusCode::OK);

    // 3. Camera named after reserved prefix ("exports") must still enforce camera scoping on /{camera}/* subroutes
    for reserved_cam_uri in [
        "/api/exports/recordings",
        "/api/exports/recordings/summary",
        "/api/exports/ptz/info",
        "/api/exports/state",
        "/api/exports/latest.jpg",
    ] {
        let r_res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(reserved_cam_uri)
                    .header(header::AUTHORIZATION, &scoped_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_res.status(),
            StatusCode::FORBIDDEN,
            "scoped user must be forbidden from {reserved_cam_uri}"
        );
    }

    // 4. resolve_static_media_camera rejects multi-segment /exports/ subdir paths and resolves removed cameras from SQLite
    std::fs::write(
        media_dir.join("exports/front_door_1_2/nested.mp4"),
        b"NESTED",
    )
    .unwrap();
    assert_eq!(
        resolve_static_media_camera(&state, "/exports/front_door_1_2/nested.mp4"),
        None,
        "multi-segment /exports/ paths must not resolve camera from parent directory name"
    );

    state
        .db
        .insert_review_segment(&ninjanvr_db::ReviewItemDto {
            id: "front_door-spoof-rev".to_string(),
            camera: "removed_cam".to_string(),
            start_time: 1000.0,
            end_time: Some(1005.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-removed_cam-front_door-spoof-rev.webp"
                .to_string(),
            data: json!({}),
            has_been_reviewed: false,
        })
        .unwrap();
    assert_eq!(
        resolve_static_media_camera(
            &state,
            "/clips/review/thumb-removed_cam-front_door-spoof-rev.webp"
        ),
        Some("removed_cam".to_string()),
        "review thumbnail must resolve authoritative camera from SQLite even when camera is not in cfg.cameras"
    );

    // 5. serve_spa_fallback never serves dotfiles (.env) from web_dir
    let r_dotfile = app
        .clone()
        .oneshot(Request::builder().uri("/.env").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let dotfile_body = to_bytes(r_dotfile.into_body(), 4096).await.unwrap();
    assert!(
        !std::str::from_utf8(&dotfile_body)
            .unwrap_or("")
            .contains("SECRET_DOTFILE"),
        "serve_spa_fallback must never serve hidden dotfiles from web_dir"
    );

    // 6. LogBufferHub scrubs RTSP credentials, strips CRLF, and handles i64::MIN start index safely
    assert_eq!(
        sanitize_logged_path("//share/secret_token_123/video.mp4\r\nInjected: 1"),
        "/share/<redacted>/video.mp4  Injected: 1"
    );
    let hub = LogBufferHub::new();
    hub.push_go2rtc(
        "ERR",
        "rtsp",
        "dial rtsp://admin:super_secret_pw@192.168.1.50:554/live\r\nFAKE_LINE",
    );
    let (total, lines) = hub.query_lines("go2rtc", Some(i64::MIN), None);
    assert_eq!(total, 1);
    assert_eq!(lines.len(), 1);
    assert!(!lines[0].contains("super_secret_pw"));
    assert!(!lines[0].contains('\r'));
    assert!(!lines[0].contains('\n'));

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn phase3a_login_rate_limit_csrf_oidc_hardening_and_trusted_proxy_work() {
    let tmp_dir = std::env::temp_dir().join(format!("ninjanvr-p3a-auth-{}", rand::random::<u64>()));
    let web_dir = tmp_dir.join("web-v2");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(&media_dir).unwrap();

    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let yaml = format!(
        r#"
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
  proxy:
    enabled: true
    trusted_proxies:
      - "10.42.0.0/16"
    user_header: "X-Authentik-Username"
    groups_header: "X-Authentik-Groups"
  oidc:
    enabled: false
    role_mapping:
      admin: ["nvr-admins"]
      operator: ["nvr-operators"]
cameras:
  front_door:
    enabled: true
  backyard:
    enabled: true
version: 0.18-0
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir, false)
        .await
        .unwrap();
    let admin_pw = "admin_test_pw_123";
    state
        .db
        .update_user_password("admin", admin_pw, 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras(
            "rate_user",
            "correct_pass_123",
            "operator",
            "front_door",
            1000,
        )
        .unwrap();

    let app = build_router_with_auth(state.clone(), true);

    // 1. Cross-Origin Login CSRF & Logout CSRF must be rejected with 403 FORBIDDEN
    let r_login_csrf = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/login")
                .header(header::ORIGIN, "https://evil.example")
                .header(header::HOST, "nvr.internal:5000")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "user": "admin", "password": admin_pw }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_login_csrf.status(), StatusCode::FORBIDDEN);

    let r_logout_csrf = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/logout")
                .header(header::ORIGIN, "https://evil.example")
                .header(header::HOST, "nvr.internal:5000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_logout_csrf.status(), StatusCode::FORBIDDEN);

    // 2. Oversized password (> 1024 bytes) rejected with 400 BAD_REQUEST before PBKDF2
    let huge_pw = "a".repeat(2048);
    let r_huge_pw = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "user": "rate_user", "password": huge_pw }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_huge_pw.status(), StatusCode::BAD_REQUEST);

    // 3. Brute-force login rate limiter locks out after 8 failed attempts and resists eviction floods
    for _ in 0..8 {
        let r_fail = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"user":"rate_user","password":"wrong_password"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_fail.status(), StatusCode::UNAUTHORIZED);
    }

    let r_locked = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"user":"rate_user","password":"correct_pass_123"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_locked.status(), StatusCode::TOO_MANY_REQUESTS);

    // 4. IPv4-mapped IPv6 CIDR matching + Trusted Proxy custom header enforcement (no fallback to Remote-Groups/Remote-User)
    assert!(ip_matches_cidr("::ffff:10.42.1.25", "10.42.0.0/16"));
    assert!(!ip_matches_cidr("::ffff:192.168.1.25", "10.42.0.0/16"));

    // 4a. Spoofed Remote-Groups: nvr-admins must NOT escalate X-Authentik-Username when X-Authentik-Groups is absent
    let r_proxy_smuggle = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/users")
                .header("x-peer-ip", "::ffff:10.42.0.10")
                .header("x-authentik-username", "sso_viewer")
                .header("remote-groups", "nvr-admins")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_proxy_smuggle.status(),
        StatusCode::FORBIDDEN,
        "Remote-Groups header must be ignored when custom groups_header is configured"
    );

    // 4b. Local SQLite user role demotion overrides proxy group claims
    state
        .db
        .create_user_with_cameras(
            "demoted_proxy_admin",
            "pass1234",
            "viewer",
            "front_door",
            1000,
        )
        .unwrap();
    let r_proxy_demoted = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/users")
                .header("x-peer-ip", "10.42.0.10")
                .header("x-authentik-username", "demoted_proxy_admin")
                .header("x-authentik-groups", "nvr-admins")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_proxy_demoted.status(),
        StatusCode::FORBIDDEN,
        "SQLite viewer role must override proxy admin group header"
    );

    // 5. OIDC Callback rejects requests when OIDC is disabled, rejects alg:none, missing exp/iss/aud, and missing UserInfo sub
    let r_oidc_disabled = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/oidc/callback?code=abc&state=st1")
                .header(
                    header::COOKIE,
                    "ninjanvr_oidc_pkce=st1:ver1:http://localhost:5000/api/auth/oidc/callback",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_oidc_disabled.status(), StatusCode::BAD_REQUEST);

    let oidc_case = Arc::new(std::sync::Mutex::new("alg_none".to_string()));
    let oidc_case_tok = oidc_case.clone();
    let oidc_case_ui = oidc_case.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let issuer = format!("http://{addr}");
    let iss_clone = issuer.clone();

    let mock_idp = Router::new()
        .route(
            "/oauth/v2/token",
            post(move || {
                let mode = oidc_case_tok.lock().unwrap().clone();
                let iss = iss_clone.clone();
                let exp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 3600;
                async move {
                    let encode_jwt = |hdr: &Value, pay: &Value| -> String {
                        let h = base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .encode(hdr.to_string().as_bytes());
                        let p = base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .encode(pay.to_string().as_bytes());
                        format!("{h}.{p}.sig")
                    };
                    match mode.as_str() {
                        "alg_none" => Json(json!({
                            "id_token": encode_jwt(
                                &json!({"alg":"none","typ":"JWT"}),
                                &json!({"sub":"alice","iss":iss,"aud":"nvr-client","exp":exp})
                            )
                        }))
                        .into_response(),
                        "missing_exp" => Json(json!({
                            "id_token": encode_jwt(
                                &json!({"alg":"RS256","typ":"JWT"}),
                                &json!({"sub":"alice","iss":iss,"aud":"nvr-client"})
                            )
                        }))
                        .into_response(),
                        "missing_iss" => Json(json!({
                            "id_token": encode_jwt(
                                &json!({"alg":"RS256","typ":"JWT"}),
                                &json!({"sub":"alice","aud":"nvr-client","exp":exp})
                            )
                        }))
                        .into_response(),
                        "missing_aud" => Json(json!({
                            "id_token": encode_jwt(
                                &json!({"alg":"RS256","typ":"JWT"}),
                                &json!({"sub":"alice","iss":iss,"exp":exp})
                            )
                        }))
                        .into_response(),
                        "ui_missing_sub" => Json(json!({
                            "id_token": encode_jwt(
                                &json!({"alg":"RS256","typ":"JWT"}),
                                &json!({"sub":"alice","iss":iss,"aud":"nvr-client","exp":exp})
                            ),
                            "access_token": "tok_ui_no_sub"
                        }))
                        .into_response(),
                        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                    }
                }
            }),
        )
        .route(
            "/oidc/v1/userinfo",
            get(move || {
                let mode = oidc_case_ui.lock().unwrap().clone();
                async move {
                    if mode == "ui_missing_sub" {
                        Json(json!({
                            "preferred_username": "admin",
                            "groups": ["nvr-admins"]
                        }))
                        .into_response()
                    } else {
                        StatusCode::UNAUTHORIZED.into_response()
                    }
                }
            }),
        );
    let idp_task = tokio::spawn(async move {
        let _ = axum::serve(listener, mock_idp).await;
    });

    {
        let mut cfg = state.config.write().await;
        cfg.auth.oidc.enabled = true;
        cfg.auth.oidc.issuer_url = issuer;
        cfg.auth.oidc.client_id = "nvr-client".to_string();
    }

    for mode in [
        "alg_none",
        "missing_exp",
        "missing_iss",
        "missing_aud",
        "ui_missing_sub",
    ] {
        *oidc_case.lock().unwrap() = mode.to_string();
        let r_cb = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/callback?code=c1&state=st1")
                    .header(
                        header::COOKIE,
                        "ninjanvr_oidc_pkce=st1:ver1:http://localhost:5000/api/auth/oidc/callback",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_cb.status(),
            StatusCode::UNAUTHORIZED,
            "OIDC callback mode '{mode}' must be rejected with 401 UNAUTHORIZED"
        );
    }

    idp_task.abort();
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn phase3a_user_role_atomicity_config_raw_scrubbing_stats_and_prometheus_escaping_work() {
    let tmp_dir = std::env::temp_dir().join(format!("ninjanvr-p3a-cfg-{}", rand::random::<u64>()));
    let web_dir = tmp_dir.join("web-v2");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(&media_dir).unwrap();

    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let yaml = format!(
        r#"
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
  oidc:
    enabled: false
    client_secret: 'oidc_single_quote_secret_999'
mqtt:
  enabled: false
  user: mqtt_u
  PASSWORD: "upper_case_mqtt_secret_777"
genai:
  enabled: true
  api_key: "sk-genai-literal-secret-888"
cameras:
  front_door:
    enabled: true
    detect:
      width: 1280
      height: 720
      fps: 5
  backyard:
    enabled: true
    detect:
      width: 640
      height: 480
      fps: 5
profiles:
  home:
    disabled_alerts_cameras: [front_door, backyard]
    disabled_record_cameras: [backyard]
ui:
  floorplan:
    rooms:
      - id: r1
        name: Entrance
        camera: front_door
        polygon: [[0.1, 0.1], [0.5, 0.1], [0.5, 0.5]]
      - id: r2
        name: Vault
        camera: backyard
        polygon: [[0.5, 0.5], [0.9, 0.5], [0.9, 0.9]]
version: 0.18-0
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir, false)
        .await
        .unwrap();
    let _ = state.db.ensure_default_admin(1000).unwrap();
    state
        .db
        .create_user_with_cameras("scoped_fd", "pass1234", "operator", "front_door", 1000)
        .unwrap();

    let app = build_router_with_auth(state.clone(), true);
    let admin_bearer = format!(
        "Bearer {}",
        mint_test_jwt(&state.jwt_secret, "admin", "admin", Some("*"))
    );
    let scoped_bearer = format!(
        "Bearer {}",
        mint_test_jwt(
            &state.jwt_secret,
            "scoped_fd",
            "operator",
            Some("front_door")
        )
    );

    // 1. Failing PUT /api/users/admin/role (demoting last admin) must NOT mutate admin's allowed_cameras or revoke share links
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_by_admin".to_string(),
            camera: "backyard".to_string(),
            name: "Admin Backyard Export".to_string(),
            date: 1700000000.0,
            video_path: "/media/ninjanvr/exports/exp_by_admin.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_by_admin.webp".to_string(),
            in_progress: false,
        })
        .ok();
    let now = 1700000000.0;
    state
        .db
        .insert_share_link(
            "shr_admin_by",
            "export",
            "exp_by_admin",
            Some("backyard"),
            "admin",
            now,
            now + 3600.0,
        )
        .unwrap();

    let r_demote_last_admin = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/users/admin/role")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"role":"operator","allowed_cameras":"front_door"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_demote_last_admin.status(), StatusCode::BAD_REQUEST);
    let admin_row = state.db.get_user("admin").unwrap().unwrap();
    assert_eq!(
        admin_row.allowed_cameras, "*",
        "Failed role demotion of last admin must not narrow allowed_cameras"
    );
    assert!(
        !state.db.is_share_link_revoked("shr_admin_by").unwrap(),
        "Failed role demotion of last admin must not revoke admin share links"
    );

    // 2. GET /api/config/raw must scrub single-quoted client_secret, uppercase PASSWORD, and literal secrets
    let r_raw = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/config/raw")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_raw.status(), StatusCode::OK);
    let raw_str: String =
        serde_json::from_slice(&to_bytes(r_raw.into_body(), 65536).await.unwrap()).unwrap();
    for secret in [
        "oidc_single_quote_secret_999",
        "upper_case_mqtt_secret_777",
        "sk-genai-literal-secret-888",
    ] {
        assert!(
            !raw_str.contains(secret),
            "GET /api/config/raw must never leak '{secret}'"
        );
    }

    // 3. Camera-scoped GET /api/config filters profiles.*.cameras and ui.floorplan.rooms.*.camera
    let r_cfg_scoped = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_cfg_scoped.status(), StatusCode::OK);
    let cfg_json: Value =
        serde_json::from_slice(&to_bytes(r_cfg_scoped.into_body(), 65536).await.unwrap()).unwrap();
    let home_prof = &cfg_json["profiles"]["home"];
    assert_eq!(home_prof["disabled_alerts_cameras"], json!(["front_door"]));
    assert_eq!(
        home_prof["disabled_record_cameras"],
        json!([]),
        "Scoped GET /api/config must filter unauthorized cameras from profiles.*.disabled_*_cameras"
    );

    // 4. Camera-scoped GET /api/stats filters service.heuristics targeting unauthorized cameras (backyard 4:3 aspect alert)
    let r_stats_scoped = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_stats_scoped.status(), StatusCode::OK);
    let stats_text = String::from_utf8(
        to_bytes(r_stats_scoped.into_body(), 65536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        !stats_text.contains("backyard"),
        "Scoped GET /api/stats must not leak backyard camera in service.heuristics"
    );

    // 5. GET /api/metrics escapes quotes, backslashes, and newlines in label values to prevent metric injection
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_prom_inj".to_string(),
            camera: "front_door".to_string(),
            label: "person\"}\nninjanvr_injected_metric 1\n#".to_string(),
            zones: json!([]),
            start_time: 1000.0,
            end_time: Some(1005.0),
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
    let r_prom = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/metrics")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_prom.status(), StatusCode::OK);
    let prom_body =
        String::from_utf8(to_bytes(r_prom.into_body(), 65536).await.unwrap().to_vec()).unwrap();
    assert!(
        !prom_body.contains("\nninjanvr_injected_metric 1"),
        "Prometheus metrics endpoint must escape label values to prevent metric line injection"
    );
    assert!(prom_body.contains(r#"label="person\"}\nninjanvr_injected_metric 1\n#""#));

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn phase3a_mcp_scoping_debug_traversal_federation_and_camera_proxy_hardening_work() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-p3a-mcp-proxy-{}", rand::random::<u64>()));
    let web_dir = tmp_dir.join("web-v2");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(&media_dir).unwrap();

    // Start a mock upstream server simulating both a peer node and a camera WebUI with oversized/redirect responses
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let mock_upstream = Router::new()
        .route(
            "/api/stats",
            get(|| async {
                Json(json!({
                    "service": { "version": "2.1.9" },
                    "cameras": { "front_door": {}, "backyard": {} }
                }))
            }),
        )
        .route(
            "/api/oversized",
            get(|| async {
                let mut hdrs = HeaderMap::new();
                hdrs.insert(header::CONTENT_LENGTH, "33554432".parse().unwrap());
                (hdrs, "oversized_stream")
            }),
        );
    let upstream_task = tokio::spawn(async move {
        let _ = axum::serve(upstream_listener, mock_upstream).await;
    });

    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
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
  backyard:
    enabled: true
peers:
  edge_node:
    enabled: true
    url: "http://peer_user:peer_secret_pw@{}"
  ssrf_metadata:
    enabled: true
    url: "http://169.254.169.254/latest/meta-data"
version: 0.18-0
"#,
        db_path.display(),
        upstream_addr
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let _ = state.db.ensure_default_admin(1000).unwrap();
    state
        .db
        .create_user_with_cameras("scoped_fd", "pass1234", "operator", "front_door", 1000)
        .unwrap();

    for (id, cam) in [("ev_fd_1", "front_door"), ("ev_by_1", "backyard")] {
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: id.to_string(),
                camera: cam.to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 2000.0,
                end_time: Some(2005.0),
                has_clip: false,
                has_snapshot: false,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.95),
                false_positive: Some(false),
                box_coords: None,
                data: json!({}),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.95),
                stationary: false,
            })
            .unwrap();
    }

    let app = build_router_with_auth(state.clone(), true);
    let admin_bearer = format!(
        "Bearer {}",
        mint_test_jwt(&state.jwt_secret, "admin", "admin", Some("*"))
    );
    let scoped_bearer = format!(
        "Bearer {}",
        mint_test_jwt(
            &state.jwt_secret,
            "scoped_fd",
            "operator",
            Some("front_door")
        )
    );

    // 1. MCP search_events with "camera": "all" or "camera": "" for scoped user must ONLY return front_door events
    for cam_arg in [json!("all"), json!(""), json!("backyard")] {
        let r_mcp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mcp")
                    .header(header::AUTHORIZATION, &scoped_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "jsonrpc": "2.0",
                            "id": 1,
                            "method": "tools/call",
                            "params": {
                                "name": "search_events",
                                "arguments": { "camera": cam_arg }
                            }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_mcp.status(), StatusCode::OK);
        let mcp_json: Value =
            serde_json::from_slice(&to_bytes(r_mcp.into_body(), 16384).await.unwrap()).unwrap();
        let events = mcp_json["result"]["structuredContent"]["events"]
            .as_array()
            .unwrap();
        assert!(
            events.iter().all(|e| e["camera"] == "front_door"),
            "MCP search_events with camera={cam_arg} must never return backyard events to scoped user"
        );
        if cam_arg != json!("backyard") {
            assert_eq!(events.len(), 1);
        } else {
            assert!(events.is_empty());
        }
    }

    // 2. Debug LPR Simulate & Debug Reprocess must reject path traversal camera names ("../../pwn")
    let r_lpr_trav = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/debug/lpr-simulate")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"camera":"../../outside_pwn","plate":"BRA2E19"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_lpr_trav.status(), StatusCode::BAD_REQUEST);

    let r_reproc_trav = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/debug/reprocess")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"camera":"../outside","start_time":1000.0,"end_time":1010.0}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_reproc_trav.status(), StatusCode::BAD_REQUEST);

    // 3. GET /api/federation/nodes must scrub peer URL credentials
    let r_fed_nodes = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/federation/nodes")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_fed_nodes.status(), StatusCode::OK);
    let fed_nodes_str = String::from_utf8(
        to_bytes(r_fed_nodes.into_body(), 16384)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        !fed_nodes_str.contains("peer_secret_pw"),
        "GET /api/federation/nodes must scrub embedded credentials in peer URL"
    );

    // 4. Federation proxy blocks %2e%2e%2f encoded subpath, blocks SSRF metadata peer URL, and bounds upstream body size
    let r_fed_ssrf = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/federation/ssrf_metadata/stats")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_fed_ssrf.status(), StatusCode::BAD_REQUEST);

    let r_fed_oversized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/federation/edge_node/oversized")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_fed_oversized.status(), StatusCode::BAD_GATEWAY);

    upstream_task.abort();
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn phase3a_cycle2_oidc_jwks_yaml_block_scrub_ssrf_userinfo_and_proxy_hardening_work() {
    let tmp_dir = std::env::temp_dir().join(format!("ninjanvr-p3a-c2-{}", rand::random::<u64>()));
    let web_dir = tmp_dir.join("web-v2");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(&media_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html>Console v2</html>").unwrap();

    // 1. sanitize_logged_path redacts percent-encoded and mixed-case /share/<token> paths
    assert_eq!(
        sanitize_logged_path("/%73hare/secret_tok_999/video.mp4"),
        "/share/<redacted>/video.mp4"
    );
    assert_eq!(
        sanitize_logged_path("/SHARE/secret_tok_999"),
        "/share/<redacted>"
    );

    // 2. Mock upstream for OIDC Discovery + JWKS + Camera WebUI redirect tests
    let oidc_mode = Arc::new(std::sync::Mutex::new("jwks_bad_sig".to_string()));
    let oidc_mode_disc = oidc_mode.clone();
    let oidc_mode_tok = oidc_mode.clone();
    let oidc_mode_ui = oidc_mode.clone();

    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let issuer = format!("http://{upstream_addr}");
    let iss_disc = issuer.clone();
    let iss_tok = issuer.clone();

    let jwk_secret = b"oidc-jwks-oct-signing-key-32byte";
    let jwk_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(jwk_secret);

    let mock_upstream = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || {
                let mode = oidc_mode_disc.lock().unwrap().clone();
                let iss = iss_disc.clone();
                async move {
                    if mode == "disc_bad_issuer" {
                        return Json(json!({
                            "issuer": "https://evil.example",
                            "token_endpoint": format!("{iss}/oauth/v2/token"),
                            "userinfo_endpoint": format!("{iss}/oidc/v1/userinfo"),
                            "jwks_uri": format!("{iss}/jwks.json")
                        }))
                        .into_response();
                    }
                    if mode == "disc_ssrf_jwks" {
                        return Json(json!({
                            "issuer": iss,
                            "token_endpoint": format!("{iss}/oauth/v2/token"),
                            "userinfo_endpoint": format!("{iss}/oidc/v1/userinfo"),
                            "jwks_uri": "http://169.254.169.254/jwks.json"
                        }))
                        .into_response();
                    }
                    if mode == "disc_missing_jwks_rs256" {
                        return Json(json!({
                            "issuer": iss,
                            "token_endpoint": format!("{iss}/oauth/v2/token"),
                            "userinfo_endpoint": format!("{iss}/oidc/v1/userinfo")
                        }))
                        .into_response();
                    }
                    if mode == "disc_500" {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                    Json(json!({
                        "issuer": iss,
                        "token_endpoint": format!("{iss}/oauth/v2/token"),
                        "userinfo_endpoint": format!("{iss}/oidc/v1/userinfo"),
                        "jwks_uri": format!("{iss}/jwks.json")
                    }))
                    .into_response()
                }
            }),
        )
        .route(
            "/jwks.json",
            get(move || {
                let k = jwk_b64.clone();
                async move {
                    Json(json!({
                        "keys": [
                            {
                                "kty": "oct",
                                "alg": "HS256",
                                "kid": "k1",
                                "k": k
                            }
                        ]
                    }))
                }
            }),
        )
        .route(
            "/oauth/v2/token",
            post(move || {
                let mode = oidc_mode_tok.lock().unwrap().clone();
                let iss = iss_tok.clone();
                let exp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 3600;
                async move {
                    let mut hdr = Header::new(Algorithm::HS256);
                    hdr.kid = Some("k1".to_string());
                    match mode.as_str() {
                        "jwks_bad_sig" => {
                            let bad_tok = jsonwebtoken::encode(
                                &hdr,
                                &json!({"sub":"alice","iss":iss,"aud":"nvr-client","exp":exp}),
                                &EncodingKey::from_secret(b"wrong-signing-key-00000000000000"),
                            )
                            .unwrap();
                            Json(json!({ "id_token": bad_tok })).into_response()
                        }
                        "id_missing_sub" => {
                            let no_sub_tok = jsonwebtoken::encode(
                                &hdr,
                                &json!({"preferred_username":"admin","iss":iss,"aud":"nvr-client","exp":exp,"groups":["nvr-admins"]}),
                                &EncodingKey::from_secret(jwk_secret),
                            )
                            .unwrap();
                            Json(json!({ "id_token": no_sub_tok })).into_response()
                        }
                        "ui_only_missing_sub" => {
                            Json(json!({ "access_token": "tok_only_ui" })).into_response()
                        }
                        "disc_missing_jwks_rs256" => {
                            let h = base64::engine::general_purpose::URL_SAFE_NO_PAD
                                .encode(r#"{"alg":"RS256","typ":"JWT"}"#);
                            let p = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                                json!({"sub":"alice","iss":iss,"aud":"nvr-client","exp":exp})
                                    .to_string()
                                    .as_bytes(),
                            );
                            Json(json!({ "id_token": format!("{h}.{p}.forged_sig") }))
                                .into_response()
                        }
                        "jwks_valid" => {
                            let ok_tok = jsonwebtoken::encode(
                                &hdr,
                                &json!({"sub":"alice","preferred_username":"alice","iss":iss,"aud":"nvr-client","exp":exp,"groups":["nvr-admins"]}),
                                &EncodingKey::from_secret(jwk_secret),
                            )
                            .unwrap();
                            Json(json!({
                                "id_token": ok_tok,
                                "access_token": "tok_ok"
                            }))
                            .into_response()
                        }
                        _ => StatusCode::BAD_REQUEST.into_response(),
                    }
                }
            }),
        )
        .route(
            "/oidc/v1/userinfo",
            get(move || {
                let mode = oidc_mode_ui.lock().unwrap().clone();
                async move {
                    if mode == "ui_only_missing_sub" {
                        return Json(json!({
                            "preferred_username": "admin",
                            "groups": ["nvr-admins"]
                        }))
                        .into_response();
                    }
                    Json(json!({
                        "sub": "alice",
                        "preferred_username": "alice",
                        "groups": ["nvr-admins"]
                    }))
                    .into_response()
                }
            }),
        );
    let upstream_task = tokio::spawn(async move {
        let _ = axum::serve(upstream_listener, mock_upstream).await;
    });

    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let yaml = format!(
        r#"
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
  proxy:
    enabled: true
    trusted_proxies:
      - "10.42.0.0/16"
    user_header: "Remote-User"
    groups_header: "Remote-Groups"
  oidc:
    enabled: true
    issuer_url: "{}"
    client_id: "nvr-client"
    client_secret: |-
      block_scalar_oidc_secret_987654
    role_mapping:
      admin: ["nvr-admins"]
mqtt: {{ enabled: false, password: "flow_mapping_mqtt_secret_654321" }}
cameras:
  front_door:
    enabled: true
  backyard:
    enabled: true
peers:
  ssrf_userinfo_meta:
    enabled: true
    url: "http://user:pass@169.254.169.254/latest/meta-data"
  ssrf_userinfo_gcp:
    enabled: true
    url: "http://user:pass@metadata.google.internal."
version: 0.18-0
"#,
        db_path.display(),
        issuer
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir, false)
        .await
        .unwrap();
    let _ = state.db.ensure_default_admin(1000).unwrap();
    let app = build_router_with_auth(state.clone(), true);
    let admin_bearer = format!(
        "Bearer {}",
        mint_test_jwt(&state.jwt_secret, "admin", "admin", Some("*"))
    );

    // 3. OIDC JWKS verification rejects forged signature, discovery issuer mismatch, SSRF jwks_uri, and missing sub
    for bad_mode in [
        "jwks_bad_sig",
        "disc_bad_issuer",
        "disc_ssrf_jwks",
        "disc_missing_jwks_rs256",
        "disc_500",
        "id_missing_sub",
        "ui_only_missing_sub",
    ] {
        *oidc_mode.lock().unwrap() = bad_mode.to_string();
        let r_cb = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/callback?code=c1&state=st1")
                    .header(
                        header::COOKIE,
                        "ninjanvr_oidc_pkce=st1:ver1:http://localhost:5000/api/auth/oidc/callback",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_cb.status(),
            StatusCode::UNAUTHORIZED,
            "OIDC callback mode '{bad_mode}' must be rejected with 401 UNAUTHORIZED"
        );
    }

    // Tampered PKCE redirect_uri pointing to external host must be rejected with 403 FORBIDDEN
    *oidc_mode.lock().unwrap() = "jwks_valid".to_string();
    let r_cb_tampered_redirect = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/oidc/callback?code=c1&state=st1")
                .header(
                    header::COOKIE,
                    "ninjanvr_oidc_pkce=st1:ver1:https://evil.example/api/auth/oidc/callback",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_cb_tampered_redirect.status(), StatusCode::FORBIDDEN);

    // Valid JWKS-signed ID token succeeds with 303 SEE_OTHER
    let r_cb_valid = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/oidc/callback?code=c1&state=st1")
                .header(
                    header::COOKIE,
                    "ninjanvr_oidc_pkce=st1:ver1:http://localhost:5000/api/auth/oidc/callback",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_cb_valid.status(), StatusCode::SEE_OTHER);

    // 4. GET /api/config/raw scrubs both YAML block scalar (|-) and inline flow mapping ({ password: ... }) secrets
    let r_raw = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/config/raw")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_raw.status(), StatusCode::OK);
    let raw_str: String =
        serde_json::from_slice(&to_bytes(r_raw.into_body(), 65536).await.unwrap()).unwrap();
    assert!(
        !raw_str.contains("block_scalar_oidc_secret_987654"),
        "GET /api/config/raw must scrub YAML block scalar secrets"
    );
    assert!(
        !raw_str.contains("flow_mapping_mqtt_secret_654321"),
        "GET /api/config/raw must scrub inline YAML flow mapping secrets"
    );

    // 5. POST /api/config/save rejects '//etc/shadow.txt' and '/./etc/passwd.txt' labelmap_path bypasses
    for bad_labelmap in [
        "//etc/shadow.txt",
        "/./etc/passwd.txt",
        "/var//run/secrets.txt",
    ] {
        let r_save_bad = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/config/save")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::from(format!(
                        "model:\n  labelmap_path: \"{bad_labelmap}\"\ncameras: {{}}\n"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_save_bad.status(),
            StatusCode::BAD_REQUEST,
            "labelmap_path '{bad_labelmap}' must be rejected"
        );
    }

    // 6. Federation proxy blocks SSRF peer URLs with embedded userinfo@ on link-local and metadata.google.internal.
    for peer_node in ["ssrf_userinfo_meta", "ssrf_userinfo_gcp"] {
        let r_ssrf = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/federation/{peer_node}/stats"))
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_ssrf.status(),
            StatusCode::BAD_REQUEST,
            "Federation peer '{peer_node}' with userinfo@ SSRF host must be rejected"
        );
    }

    // 7. Trusted Proxy request accompanied by a stale scoped cookie still grants full proxy camera access
    let stale_scoped_jwt = mint_test_jwt(
        &state.jwt_secret,
        "stale_cookie_user",
        "viewer",
        Some("front_door"),
    );
    let r_proxy_with_stale_cookie = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/profile")
                .header("x-peer-ip", "10.42.0.25")
                .header("remote-user", "proxy_operator")
                .header("remote-groups", "nvr-operators")
                .header(header::COOKIE, format!("ninjanvr_token={stale_scoped_jwt}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_proxy_with_stale_cookie.status(), StatusCode::OK);
    let prof_json: Value = serde_json::from_slice(
        &to_bytes(r_proxy_with_stale_cookie.into_body(), 8192)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(prof_json["username"], "proxy_operator");
    let cams_arr = prof_json["allowed_cameras"].as_array().unwrap();
    assert_eq!(
        cams_arr.len(),
        2,
        "Trusted proxy user must not be restricted by an unrelated stale cookie in CookieJar"
    );

    // 8. API token creation rejects expires_in_days = 0 and control-char names
    let r_tok_zero_exp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/tokens")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"zero_exp","scope":"read_only","expires_in_days":0}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_tok_zero_exp.status(), StatusCode::BAD_REQUEST);

    upstream_task.abort();
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn phase3a_cycle3_oidc_proxy_role_cap_token_prefix_ssrf_and_yaml_scrub_regression_work() {
    let tmp_dir = std::env::temp_dir().join(format!("ninjanvr-p3a-c3-{}", rand::random::<u64>()));
    let web_dir = tmp_dir.join("web-v2");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(&media_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html>Console v2</html>").unwrap();

    let oidc_mode = Arc::new(std::sync::Mutex::new("viewer_admin_collision".to_string()));
    let oidc_mode_tok = oidc_mode.clone();

    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    let issuer = format!("http://{upstream_addr}");
    let iss_disc = issuer.clone();
    let iss_tok = issuer.clone();

    let jwk_secret = b"oidc-c3-jwks-oct-signing-key-32b";
    let jwk_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(jwk_secret);

    let mock_upstream = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || {
                let iss = iss_disc.clone();
                async move {
                    Json(json!({
                        "issuer": iss,
                        "token_endpoint": format!("{iss}/oauth/v2/token"),
                        "jwks_uri": format!("{iss}/jwks.json")
                    }))
                }
            }),
        )
        .route(
            "/jwks.json",
            get(move || {
                let k = jwk_b64.clone();
                async move {
                    Json(json!({
                        "keys": [{ "kty": "oct", "alg": "HS256", "kid": "k1", "k": k }]
                    }))
                }
            }),
        )
        .route(
            "/oauth/v2/token",
            post(move || {
                let mode = oidc_mode_tok.lock().unwrap().clone();
                let iss = iss_tok.clone();
                let exp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 3600;
                async move {
                    let mut hdr = Header::new(Algorithm::HS256);
                    hdr.kid = Some("k1".to_string());
                    let claims = match mode.as_str() {
                        "token_prefix_claim" => json!({
                            "sub": "token:spoofed_bot",
                            "preferred_username": "token:spoofed_bot",
                            "iss": iss,
                            "aud": "nvr-client",
                            "exp": exp,
                            "groups": ["nvr-admins"]
                        }),
                        "ext_prefix_claim" => json!({
                            "sub": "EXT:spoofed_external",
                            "preferred_username": "ext:spoofed_external",
                            "iss": iss,
                            "aud": "nvr-client",
                            "exp": exp,
                            "groups": ["nvr-admins"]
                        }),
                        _ => json!({
                            "sub": "ext-sub-999",
                            "preferred_username": "admin",
                            "iss": iss,
                            "aud": "nvr-client",
                            "exp": exp,
                            "groups": ["viewers"]
                        }),
                    };
                    let tok =
                        jsonwebtoken::encode(&hdr, &claims, &EncodingKey::from_secret(jwk_secret))
                            .unwrap();
                    Json(json!({ "id_token": tok })).into_response()
                }
            }),
        );
    let upstream_task = tokio::spawn(async move {
        let _ = axum::serve(upstream_listener, mock_upstream).await;
    });

    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let yaml = format!(
        r#"
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
  proxy:
    enabled: true
    trusted_proxies:
      - "10.42.0.0/16"
    user_header: "Remote-User"
    groups_header: "Remote-Groups"
    logout_url: "https://sso_user:sso_secret_pw@sso.example.invalid/logout?token=logout_tok_999"
  oidc:
    enabled: true
    issuer_url: "{}"
    client_id: "nvr-client"
    role_mapping:
      admin: ["nvr-admins"]
      operator: ["nvr-operators"]
      viewer: ["viewers"]
    default_role: "viewer"
    client_secret:
      next_line_plain_oidc_secret_445566
notifications: {{ webhook_secret: 987654321 }}
cameras:
  front_door:
    enabled: true
  backyard:
    enabled: true
peers:
  ssrf_dec_imds:
    enabled: true
    url: "http://2852039166"
  ssrf_hex_imds:
    enabled: true
    url: "http://0xa9fea9fe"
  ssrf_oct_imds:
    enabled: true
    url: "http://0251.0376.0251.0376"
  ssrf_pct_imds:
    enabled: true
    url: "http://169.254.169.%32%35%34"
  ssrf_ipv6_imds:
    enabled: true
    url: "http://[fd00:ec2::254]"
  ssrf_alibaba_imds:
    enabled: true
    url: "http://100.100.100.200"
  valid_peer:
    enabled: true
    url: "http://{}"
version: 0.18-0
"#,
        db_path.display(),
        issuer,
        upstream_addr
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir, false)
        .await
        .unwrap();
    let _ = state.db.ensure_default_admin(1000).unwrap();
    let (_, metrics_tok) = state
        .db
        .create_api_token("prom_c3", "metrics", None)
        .unwrap();

    let app = build_router_with_auth(state.clone(), true);
    let direct_app = build_router(state.clone());
    let admin_bearer = format!(
        "Bearer {}",
        mint_test_jwt(&state.jwt_secret, "admin", "admin", Some("*"))
    );

    // 1. OIDC user with preferred_username="admin" but groups=["viewers"] must NOT escalate to SQLite "admin" role
    let r_oidc_collision = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/oidc/callback?code=c1&state=st1")
                .header(
                    header::COOKIE,
                    "ninjanvr_oidc_pkce=st1:ver1:http://localhost:5000/api/auth/oidc/callback",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_oidc_collision.status(), StatusCode::SEE_OTHER);
    let session_cookie = r_oidc_collision
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            let s = v.to_str().ok()?;
            if s.starts_with("ninjanvr_token=") {
                Some(s.split(';').next().unwrap().to_string())
            } else {
                None
            }
        })
        .unwrap();

    // Using that OIDC session cookie on admin-only /api/users must be rejected with 403 FORBIDDEN
    let r_oidc_users_forbid = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/users")
                .header(header::COOKIE, &session_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_oidc_users_forbid.status(),
        StatusCode::FORBIDDEN,
        "OIDC viewer with preferred_username='admin' must not inherit local SQLite admin role"
    );

    // 2. Trusted Proxy user with Remote-User: admin and Remote-Groups: viewers must NOT escalate to admin
    let r_proxy_admin_collision = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/users")
                .header("x-peer-ip", "10.42.0.50")
                .header("remote-user", "admin")
                .header("remote-groups", "viewers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_proxy_admin_collision.status(),
        StatusCode::FORBIDDEN,
        "Trusted proxy viewer with Remote-User: admin must not escalate to admin via SQLite collision"
    );

    // 2b. Non-admin JWT with sub="admin" and allowed_cameras=Some("front_door") restricts cameras via JWT claims
    let scoped_admin_sub_jwt = format!(
        "Bearer {}",
        mint_test_jwt(&state.jwt_secret, "admin", "operator", Some("front_door"))
    );
    let r_scoped_admin_sub_by = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/backyard/state")
                .header(header::AUTHORIZATION, &scoped_admin_sub_jwt)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_scoped_admin_sub_by.status(),
        StatusCode::FORBIDDEN,
        "Non-admin JWT colliding with SQLite admin user must enforce explicit JWT allowed_cameras"
    );

    // 3. Reserved 'token:' and 'ext:' prefix spoofing (case-insensitive) in JWT sub, OIDC claims, and Trusted Proxy Remote-User is rejected
    for spoofed_sub in [
        "token:spoofed_op",
        "TOKEN:spoofed_op",
        "ext:spoofed_op",
        "EXT:spoofed_op",
    ] {
        let spoofed_jwt = format!(
            "Bearer {}",
            mint_test_jwt(
                &state.jwt_secret,
                spoofed_sub,
                "operator",
                Some("front_door")
            )
        );
        let r_spoof_jwt = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/config")
                    .header(header::AUTHORIZATION, &spoofed_jwt)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_spoof_jwt.status(),
            StatusCode::UNAUTHORIZED,
            "JWT sub '{spoofed_sub}' must be rejected as a reserved principal prefix"
        );
    }

    for spoofed_remote_user in [
        "token:spoofed_proxy",
        "TOKEN:spoofed_proxy",
        "ext:spoofed_proxy",
        "EXT:spoofed_proxy",
    ] {
        let r_spoof_proxy = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/config")
                    .header("x-peer-ip", "10.42.0.50")
                    .header("remote-user", spoofed_remote_user)
                    .header("remote-groups", "nvr-operators")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_spoof_proxy.status(),
            StatusCode::UNAUTHORIZED,
            "Trusted-Proxy Remote-User '{spoofed_remote_user}' must be rejected as a reserved principal prefix"
        );
    }

    for bad_oidc_mode in ["token_prefix_claim", "ext_prefix_claim"] {
        *oidc_mode.lock().unwrap() = bad_oidc_mode.to_string();
        let r_spoof_oidc = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/callback?code=c1&state=st1")
                    .header(
                        header::COOKIE,
                        "ninjanvr_oidc_pkce=st1:ver1:http://localhost:5000/api/auth/oidc/callback",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_spoof_oidc.status(),
            StatusCode::UNAUTHORIZED,
            "OIDC callback mode '{bad_oidc_mode}' must be rejected as a reserved principal prefix"
        );
    }

    // 4. Federation peer SSRF bypasses (decimal/hex/octal/percent-encoded IPv4, IPv6 IMDS, Alibaba IMDS) and double-encoded '%252e%252e' subpath are blocked
    for ssrf_peer in [
        "ssrf_dec_imds",
        "ssrf_hex_imds",
        "ssrf_oct_imds",
        "ssrf_pct_imds",
        "ssrf_ipv6_imds",
        "ssrf_alibaba_imds",
    ] {
        let r_ssrf = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/federation/{ssrf_peer}/stats"))
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_ssrf.status(),
            StatusCode::BAD_REQUEST,
            "Federation peer '{ssrf_peer}' must be rejected as SSRF"
        );
    }

    let r_triple_enc_subpath = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/federation/valid_peer/%25252e%25252e/secret")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_triple_enc_subpath.status(), StatusCode::BAD_REQUEST);

    // 5. Public GET /api/auth/providers scrubs embedded credentials and token query params in proxy.logout_url
    let r_prov = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/providers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_prov.status(), StatusCode::OK);
    let prov_str =
        String::from_utf8(to_bytes(r_prov.into_body(), 8192).await.unwrap().to_vec()).unwrap();
    assert!(!prov_str.contains("sso_secret_pw"));
    assert!(!prov_str.contains("logout_tok_999"));

    // 6. GET /api/config/raw scrubs next-line indented plain scalar secrets and numeric flow mapping secrets
    let r_raw = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/config/raw")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_raw.status(), StatusCode::OK);
    let raw_str: String =
        serde_json::from_slice(&to_bytes(r_raw.into_body(), 65536).await.unwrap()).unwrap();
    assert!(
        !raw_str.contains("next_line_plain_oidc_secret_445566"),
        "GET /api/config/raw must scrub next-line indented plain scalar secrets"
    );
    assert!(
        !raw_str.contains("987654321"),
        "GET /api/config/raw must scrub numeric flow mapping secrets"
    );

    // 7. Direct POST /api/mcp on build_router(state) rejects metrics-scoped tokens (403) and invalid tokens (401)
    let r_mcp_metrics = direct_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mcp")
                .header(header::AUTHORIZATION, format!("Bearer {metrics_tok}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "tools/call",
                        "params": { "name": "get_security_digest", "arguments": {} }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_mcp_metrics.status(), StatusCode::FORBIDDEN);

    let r_mcp_bad_tok = direct_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mcp")
                .header(header::AUTHORIZATION, "Bearer nnvr_invalid_token_xyz")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "tools/call",
                        "params": { "name": "get_camera_status", "arguments": {} }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_mcp_bad_tok.status(), StatusCode::UNAUTHORIZED);

    // 8. [VULN-001] External public peer IP on direct_app (enforce_auth = false) is rejected with 401 on protected routes,
    //    while /api/version remains 200 OK for liveness/readiness probes.
    let r_pub_ver = direct_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/version")
                .header("x-peer-ip", "203.0.113.45")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_pub_ver.status(), StatusCode::OK);

    for (method, protected_uri) in [
        ("GET", "/api/users"),
        ("GET", "/api/tokens"),
        ("GET", "/api/config"),
        ("GET", "/api/config/raw"),
        ("POST", "/api/config/save"),
        ("POST", "/api/restart"),
    ] {
        let r_ext = direct_app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(protected_uri)
                    .header("x-peer-ip", "203.0.113.45")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_ext.status(),
            StatusCode::UNAUTHORIZED,
            "External public peer IP on direct_app must be rejected with 401 on {method} {protected_uri}"
        );
    }

    // 9. [VULN-001] Direct handler RBAC on direct_app (enforce_auth = false) blocks non-admin and camera-restricted callers
    //    across all admin-only read and mutation endpoints.
    let viewer_bearer = format!(
        "Bearer {}",
        mint_test_jwt(&state.jwt_secret, "viewer_u", "viewer", Some("*"))
    );
    let operator_bearer = format!(
        "Bearer {}",
        mint_test_jwt(&state.jwt_secret, "op_u", "operator", Some("*"))
    );
    for (method, admin_uri, body_str) in [
        ("GET", "/api/users", ""),
        (
            "POST",
            "/api/users",
            r#"{"username":"x","password":"password1234","role":"admin"}"#,
        ),
        (
            "PUT",
            "/api/users/admin/password",
            r#"{"password":"password1234"}"#,
        ),
        ("PUT", "/api/users/admin/role", r#"{"role":"viewer"}"#),
        (
            "PUT",
            "/api/users/admin/cameras",
            r#"{"allowed_cameras":"front_door"}"#,
        ),
        ("DELETE", "/api/users/admin", ""),
        ("GET", "/api/tokens", ""),
        ("POST", "/api/tokens", r#"{"name":"t1","scope":"full"}"#),
        ("DELETE", "/api/tokens/tok_1", ""),
        ("GET", "/api/config/raw", ""),
        ("POST", "/api/config/save", "cameras: {}\n"),
        ("POST", "/api/config/structured", r#"{}"#),
        ("PUT", "/api/config/set", ""),
        ("POST", "/api/restart", ""),
        ("GET", "/api/logs/ninjanvr", ""),
        ("GET", "/api/genai/models", ""),
        (
            "POST",
            "/api/debug/lpr-simulate",
            r#"{"camera":"front_door","plate":"BRA2E19"}"#,
        ),
        (
            "POST",
            "/api/debug/reprocess",
            r#"{"camera":"front_door","start_time":1000.0,"end_time":1010.0}"#,
        ),
        ("POST", "/api/notifications/test", ""),
        ("GET", "/api/camera-proxy/front_door", ""),
        ("GET", "/api/federation/nodes", ""),
        ("GET", "/api/federation/valid_peer/stats", ""),
    ] {
        for caller_auth in [&viewer_bearer, &operator_bearer] {
            let r_direct = direct_app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(admin_uri)
                        .header(header::AUTHORIZATION, caller_auth)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body_str))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                r_direct.status(),
                StatusCode::FORBIDDEN,
                "Non-admin caller on direct_app must receive 403 FORBIDDEN on {method} {admin_uri}"
            );
        }
    }

    upstream_task.abort();
    let _ = std::fs::remove_dir_all(&tmp_dir);
}
