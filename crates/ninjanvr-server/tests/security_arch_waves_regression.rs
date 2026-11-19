#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use ninjanvr_config::{Go2RtcSupervisor, NinjaConfig, scrub_yaml_credentials};
use ninjanvr_db::RecordingRow;
use ninjanvr_server::auth_gate::JwtClaims;
use ninjanvr_server::live_gateway::{
    InMemoryGo2RtcUpstream, LiveStreamGateway, ws_message_contains_webrtc_talkback,
};
use ninjanvr_server::mse_hub::MseStreamHub;
use ninjanvr_server::{
    build_metrics_router, build_router_with_auth, load_or_generate_tls_config, new_app_state,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tower::ServiceExt;

fn unix_now_sec() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn mint_jwt(secret: &str, sub: &str, role: &str, allowed_cameras: Option<&str>) -> String {
    let now = unix_now_sec();
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &JwtClaims {
            sub: sub.to_string(),
            role: role.to_string(),
            allowed_cameras: allowed_cameras.map(str::to_string),
            iat: now,
            exp: now + 3600,
        },
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

#[tokio::test]
async fn deleted_local_users_are_revoked_immediately_even_when_oidc_enabled() {
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr-sec-wave-del-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::write(
        web_dir.join("index.html"),
        "<html><body>Console</body></html>",
    )
    .unwrap();

    let yaml = format!(
        r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
  oidc:
    enabled: true
    issuer_url: https://idp.example.invalid
    client_id: ninjanvr-client
cameras:
  front_door:
    enabled: true
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, tmp_dir.clone(), false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("bob_op", "CorrectHorse1!", "operator", "*", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("alice_adm", "CorrectHorse2!", "admin", "*", 1000)
        .unwrap();

    let app = build_router_with_auth(state.clone(), true);
    let root_admin_bearer = format!(
        "Bearer {}",
        mint_jwt(&state.jwt_secret, "admin", "admin", Some("*"))
    );

    // Both bob_op and alice_adm log in via POST /api/login and obtain signed session cookies
    let login_cookie = |user: &str, pass: &str| {
        let app_c = app.clone();
        let body = json!({ "user": user, "password": pass }).to_string();
        async move {
            let resp = app_c
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/login")
                        .header(header::HOST, "nvr.example.test")
                        .header(header::ORIGIN, "https://nvr.example.test")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let set_cookie = resp
                .headers()
                .get(header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap();
            set_cookie.split(';').next().unwrap().to_string()
        }
    };

    let bob_cookie = login_cookie("bob_op", "CorrectHorse1!").await;
    let alice_cookie = login_cookie("alice_adm", "CorrectHorse2!").await;

    // Pre-deletion: both session cookies authenticate cleanly
    for cookie in [&bob_cookie, &alice_cookie] {
        let ok_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/profile")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok_resp.status(), StatusCode::OK);
    }

    // Delete both users via DELETE /api/users/{username}
    for target in ["bob_op", "alice_adm"] {
        let del_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/users/{target}"))
                    .header(header::AUTHORIZATION, &root_admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del_resp.status(), StatusCode::OK);
    }

    // Post-deletion: both cookies MUST fail closed with 401 UNAUTHORIZED even though OIDC is enabled
    for cookie in [&bob_cookie, &alice_cookie] {
        let rev_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/profile")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            rev_resp.status(),
            StatusCode::UNAUTHORIZED,
            "Deleted local user session cookie must be immediately rejected"
        );
    }

    // Re-creating `bob_op` and logging in within the same Unix second must succeed for the new cookie
    // while keeping the old pre-deletion cookie strictly revoked.
    state
        .db
        .create_user_with_cameras("bob_op", "NewPassword99!", "operator", "*", 1000)
        .unwrap();
    let bob_new_cookie = login_cookie("bob_op", "NewPassword99!").await;
    let r_old = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/profile")
                .header(header::COOKIE, &bob_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_old.status(), StatusCode::UNAUTHORIZED);
    let r_new = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/profile")
                .header(header::COOKIE, &bob_new_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_new.status(), StatusCode::OK);

    state.shutdown().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn camera_and_federation_proxies_sandbox_html_block_federated_xss_and_forward_post() {
    let mock_peer_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = mock_peer_listener.local_addr().unwrap();
    let mock_peer = Router::new()
        .route(
            "/api/evil-html",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    "<script>fetch('/api/tokens',{method:'POST'})</script>",
                )
            }),
        )
        .route(
            "/api/echo-post",
            post(|body: axum::body::Bytes| async move {
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    Json(json!({ "received_len": body.len(), "method": "POST" })),
                )
            }),
        );
    let peer_task = tokio::spawn(async move {
        let _ = axum::serve(mock_peer_listener, mock_peer).await;
    });

    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr-sec-wave-proxy-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::write(
        web_dir.join("index.html"),
        "<html><body>Console</body></html>",
    )
    .unwrap();

    let yaml = format!(
        r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
peers:
  remote_site:
    enabled: true
    url: http://127.0.0.1:{}
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://admin:secret@192.0.2.10:554/stream1
          roles: [record]
        - path: rtsp://admin:secret@192.0.2.10:554/stream2
          roles: [detect]
"#,
        db_path.display(),
        peer_addr.port()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, tmp_dir.clone(), false)
        .await
        .unwrap();
    state.enable_ephemeral_loopback_federation_for_test();
    state
        .db
        .create_user_with_cameras("viewer_one", "ViewerPass1!", "viewer", "*", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("op_one", "OperatorPass1!", "operator", "*", 1000)
        .unwrap();

    let app = build_router_with_auth(state.clone(), true);
    let metrics_app = build_metrics_router(state.clone());
    let admin_bearer = format!(
        "Bearer {}",
        mint_jwt(&state.jwt_secret, "admin", "admin", Some("*"))
    );
    let viewer_bearer = format!(
        "Bearer {}",
        mint_jwt(&state.jwt_secret, "viewer_one", "viewer", Some("*"))
    );
    let op_bearer = format!(
        "Bearer {}",
        mint_jwt(&state.jwt_secret, "op_one", "operator", Some("*"))
    );

    // 1. Federated peer returning text/html is rejected with 502 BAD_GATEWAY
    let r_fed_html = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/federation/remote_site/evil-html")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_fed_html.status(), StatusCode::BAD_GATEWAY);

    // 2. Federated POST forwards method & body and injects strict CSP + nosniff
    let r_fed_post = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/federation/remote_site/echo-post")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ping":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_fed_post.status(), StatusCode::OK);
    assert_eq!(
        r_fed_post
            .headers()
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert!(
        r_fed_post
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s.contains("sandbox"))
    );
    let post_json: Value =
        serde_json::from_slice(&to_bytes(r_fed_post.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(post_json["method"], "POST");
    assert_eq!(post_json["received_len"], 13);

    // 3. GET /api/{camera}/logs enforces Admin-only RBAC (blocks viewer and operator)
    for non_admin in [&viewer_bearer, &op_bearer] {
        let r_logs = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/front_door/logs?timeout_ms=50")
                    .header(header::AUTHORIZATION, non_admin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_logs.status(), StatusCode::FORBIDDEN);
    }

    // 4. Viewer is blocked from opening talkback WebSocket `/live/webrtc/api/ws?src=front_door_talk`
    let r_viewer_talk = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/live/webrtc/api/ws?src=front_door_talk")
                .header(header::AUTHORIZATION, &viewer_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_viewer_talk.status(), StatusCode::FORBIDDEN);

    // WebRTC WS SDP talkback detector blocks send-capable audio, case-insensitive Go JSON keys,
    // conflicting pion/webrtc `a=sendonly` + `a=recvonly` direction pairs, and non-allowlisted types
    assert!(ws_message_contains_webrtc_talkback(
        r#"{"type":"webrtc","value":"v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\n"}"#
    ));
    assert!(ws_message_contains_webrtc_talkback(
        r#"{"Type":"webrtc","Value":{"SDP":"v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\n"}}"#
    ));
    assert!(ws_message_contains_webrtc_talkback(
        r#"{"type":"webrtc","value":"v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\na=recvonly\r\n"}"#
    ));
    assert!(ws_message_contains_webrtc_talkback(
        r#"{"type":"webrtc","value":"v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\na=sendonly:1\r\n"}"#
    ));
    assert!(ws_message_contains_webrtc_talkback(
        r#"{"type":"webrtc","value":"v=0\rm=audio 9 UDP/TLS/RTP/SAVPF 111\ra=recvonly:1\r"}"#
    ));
    assert!(ws_message_contains_webrtc_talkback(
        r#"{"type":"webrtc/offer","value":"v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=recvonly\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendrecv\r\n"}"#
    ));
    assert!(!ws_message_contains_webrtc_talkback(
        r#"{"type":"webrtc","value":"v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=recvonly\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\n"}"#
    ));
    assert!(!ws_message_contains_webrtc_talkback(
        r#"{"Type":"webrtc/candidate","Value":"candidate:1 1 UDP 2130706431 192.0.2.1 50000 typ host"}"#
    ));

    // 5. Global Security Headers on SPA `/login` and explicit `/index.html`, plus `/readyz` probe
    for spa_uri in ["/login", "/index.html"] {
        let r_spa = app
            .clone()
            .oneshot(Request::builder().uri(spa_uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r_spa.status(), StatusCode::OK);
        assert_eq!(
            r_spa
                .headers()
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
        assert_eq!(
            r_spa
                .headers()
                .get(header::X_FRAME_OPTIONS)
                .and_then(|v| v.to_str().ok()),
            Some("DENY")
        );
        assert!(
            r_spa
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|csp| csp.contains("script-src 'self'")
                    && csp.contains("worker-src 'self' blob:")),
            "SPA route {spa_uri} must carry SPA_CONTENT_SECURITY_POLICY"
        );
    }

    for router in [&app, &metrics_app] {
        let r_ready = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_ready.status(), StatusCode::OK);
        let ready_json: Value =
            serde_json::from_slice(&to_bytes(r_ready.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(ready_json["ready"], true);
        assert_eq!(ready_json["database"], true);
    }

    // 6. Multipart model upload > 2 MiB (3 MiB payload) is not truncated/rejected by Axum's default 2 MiB limit
    let boundary = "----NinjaNvrModelUploadBoundary";
    let mut mp_body = Vec::with_capacity(3 * 1024 * 1024 + 512);
    mp_body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"large_test.onnx\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    mp_body.resize(mp_body.len() + 3 * 1024 * 1024, b'M');
    mp_body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let r_upload = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/detectors/models/upload")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(mp_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        r_upload.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "3 MiB multipart model upload must not hit Axum's 2 MiB DefaultBodyLimit"
    );

    peer_task.abort();
    state.shutdown().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn go2rtc_loopback_defaults_supervisor_shutdown_yaml_scrub_tls_persistence_and_vod_invalidation_work()
 {
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr-sec-wave-core-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&tmp_dir).unwrap();

    // 1. Multiline flow YAML and JSON secret scrubbing (including trailing sibling secret keys after comma)
    let raw_flow = "mqtt: { \"password\":\n  \"FlowSecretValue99\", \"api_key\": \"TrailingSecret77\" }\n{\n  \"api_key\":\n  \"JsonSameIndentSecret88\"\n}\n";
    let scrubbed = scrub_yaml_credentials(raw_flow);
    assert!(
        !scrubbed.contains("FlowSecretValue99"),
        "flow secret leaked: {scrubbed}"
    );
    assert!(
        !scrubbed.contains("TrailingSecret77"),
        "trailing flow sibling secret leaked: {scrubbed}"
    );
    assert!(
        !scrubbed.contains("JsonSameIndentSecret88"),
        "same-indent JSON secret leaked: {scrubbed}"
    );

    // 2. Credentialed RTSP inputs route via local go2rtc loopback when go2rtc relay is active
    let cfg_yaml = r#"
go2rtc:
  streams:
    front_door:
      - rtsp://camadmin:SuperSecretCamPw@192.0.2.50:554/stream1
    front_door_sub:
      - rtsp://camadmin:SuperSecretCamPw@192.0.2.50:554/stream2
cameras:
  front_door:
    enabled: true
"#;
    let cfg = NinjaConfig::from_yaml_str(cfg_yaml, tmp_dir.join("config.yml")).unwrap();
    assert_eq!(
        cfg.prefer_local_go2rtc_relay_url(
            "front_door",
            "record",
            "rtsp://camadmin:SuperSecretCamPw@192.0.2.50:554/stream1",
            true,
        ),
        "rtsp://127.0.0.1:8554/front_door"
    );
    assert_eq!(
        cfg.prefer_local_go2rtc_relay_url(
            "front_door",
            "detect",
            "rtsp://camadmin:SuperSecretCamPw@192.0.2.50:554/stream2",
            true,
        ),
        "rtsp://127.0.0.1:8554/front_door_sub"
    );

    let cfg_custom_rtsp_port = NinjaConfig::from_yaml_str(
        r#"
go2rtc:
  rtsp:
    listen: "127.0.0.1:18554"
  streams:
    front_door:
      - rtsp://camadmin:SuperSecretCamPw@192.0.2.50:554/stream1
"#,
        tmp_dir.join("config_port.yml"),
    )
    .unwrap();
    assert_eq!(
        cfg_custom_rtsp_port.prefer_local_go2rtc_relay_url(
            "front_door",
            "record",
            "rtsp://camadmin:SuperSecretCamPw@192.0.2.50:554/stream1",
            true,
        ),
        "rtsp://127.0.0.1:18554/front_door"
    );

    let cfg_auth_rtsp = NinjaConfig::from_yaml_str(
        r#"
go2rtc:
  rtsp:
    listen: "127.0.0.1:8554"
    username: "relayuser"
    password: "relaypassword"
  streams:
    front_door:
      - rtsp://camadmin:SuperSecretCamPw@192.0.2.50:554/stream1
"#,
        tmp_dir.join("config_auth_rtsp.yml"),
    )
    .unwrap();
    assert_eq!(
        cfg_auth_rtsp.prefer_local_go2rtc_relay_url(
            "front_door",
            "record",
            "rtsp://camadmin:SuperSecretCamPw@192.0.2.50:554/stream1",
            true,
        ),
        "rtsp://camadmin:SuperSecretCamPw@192.0.2.50:554/stream1"
    );

    // 3. Deterministic Go2RtcSupervisor::shutdown stops child and removes shm config without respawning
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let shm_path = tmp_dir.join("go2rtc_shm.yaml");
        let hk_path = tmp_dir.join("go2rtc_hk.yml");
        let fake_bin = tmp_dir.join("fake_go2rtc_sleep.sh");
        std::fs::write(&fake_bin, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let sup = Arc::new(Go2RtcSupervisor::with_paths(
            Arc::new(RwLock::new(cfg.clone())),
            fake_bin,
            shm_path.clone(),
            hk_path,
        ));
        sup.clone().spawn_supervisor_task();
        for _ in 0..40 {
            if sup.pid() != 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_ne!(sup.pid(), 0);
        assert!(shm_path.exists());

        sup.shutdown().await;
        assert_eq!(sup.pid(), 0);
        assert!(!sup.is_running());
        assert!(
            !shm_path.exists(),
            "Go2RtcSupervisor::shutdown must scrub and remove shm go2rtc.yaml"
        );
    }

    // 4. Persistent self-signed TLS cert reuse in config_dir + LiveStreamGateway setup
    let tls_dir = tmp_dir.join("tls_cfg");
    std::fs::create_dir_all(&tls_dir).unwrap();
    let sans = vec!["localhost".to_string()];
    let _tls_a = load_or_generate_tls_config(&cfg.tls, &tls_dir, &sans).unwrap();
    let cert_first = std::fs::read(tls_dir.join(".tls_cert.pem")).unwrap();
    let _tls_b = load_or_generate_tls_config(&cfg.tls, &tls_dir, &sans).unwrap();
    let cert_second = std::fs::read(tls_dir.join(".tls_cert.pem")).unwrap();
    assert_eq!(
        cert_first, cert_second,
        "Persisted self-signed TLS cert must be reused across restarts when SANs are unchanged"
    );
    let partial_tls = ninjanvr_config::TlsConfig {
        enabled: true,
        cert_path: Some(tls_dir.join(".tls_cert.pem")),
        key_path: None,
        ..Default::default()
    };
    assert!(
        load_or_generate_tls_config(&partial_tls, &tls_dir, &sans).is_err(),
        "Partial TLS config (cert_path without key_path) must fail closed"
    );

    // 5. VodWindowCache invalidation seq advances on compaction and deletion, not on unrelated camera segment inserts
    let db = ninjanvr_db::NinjaDatabase::open(tmp_dir.join("vod_seq.db")).unwrap();
    let seq0 = db.recordings_vod_invalidation_seq();
    db.insert_recording(&RecordingRow {
        id: "seg_1".to_string(),
        camera: "front_door".to_string(),
        path: "/media/ninjanvr/recordings/front_door/seg_1.mp4".to_string(),
        start_time: 1000.0,
        end_time: 1010.0,
        duration: 10.0,
        motion: 0,
        objects: 0,
        dbfs: -40,
        segment_size: 2.5,
        regions: 0,
        motion_heatmap: None,
    })
    .unwrap();
    assert_eq!(
        db.recordings_vod_invalidation_seq(),
        seq0,
        "Routine 10s segment insert must not invalidate unrelated VodWindowCache entries"
    );
    db.update_recording_compacted("seg_1", 1.1).unwrap();
    let seq1 = db.recordings_vod_invalidation_seq();
    assert!(
        seq1 > seq0,
        "Cold compaction must advance recordings_vod_invalidation_seq to evict stale VodWindowCache entries"
    );
    db.delete_recording_by_id("seg_1").unwrap();
    assert!(
        db.recordings_vod_invalidation_seq() > seq1,
        "delete_recording_by_id must advance recordings_vod_invalidation_seq"
    );

    let gw = LiveStreamGateway::new(
        Arc::new(RwLock::new(cfg)),
        MseStreamHub::new(),
        Arc::new(InMemoryGo2RtcUpstream::default()),
    );
    assert!(gw.known_camera_set().await.contains("front_door"));

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
