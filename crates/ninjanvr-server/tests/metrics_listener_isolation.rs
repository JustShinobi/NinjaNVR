use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use ninjanvr_config::{NINJANVR_VERSION, is_safe_secret_env_var_name};
use ninjanvr_server::{
    DEFAULT_METRICS_PORT, JwtClaims, build_metrics_router, build_router_with_auth,
    is_disallowed_federation_port, is_protected_path, new_app_state, validate_genai_base_url,
};
use serde_json::json;
use tokio::net::TcpListener;
use tower::ServiceExt;

#[tokio::test]
async fn dedicated_metrics_listener_serves_unauthenticated_openmetrics_while_app_router_stays_fail_closed()
 {
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr-metrics-isolation-{}",
        rand::random::<u64>()
    ));
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(media_dir.join("clips")).unwrap();
    std::fs::create_dir_all(media_dir.join("recordings")).unwrap();
    std::fs::create_dir_all(media_dir.join("exports")).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html>Console</html>").unwrap();
    std::fs::write(media_dir.join("clips/front_door-ev1.webp"), b"WEBP").unwrap();
    std::fs::write(media_dir.join("recordings/seg.mp4"), b"MP4").unwrap();
    std::fs::write(media_dir.join("exports/exp.mp4"), b"MP4").unwrap();

    let cold_dir = tmp_dir.join("recordings_cold_archive");
    std::fs::create_dir_all(&cold_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let yaml = format!(
        r#"
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
record:
  storage_tiers:
    enabled: true
    cold_dir: {}
go2rtc:
  streams:
    front_door:
      - "rtsp://camadmin:SuperSecretRtspPass123@192.0.2.50:554/stream1"
    backyard:
      - "rtsp://camadmin:AnotherSecretPass456@192.0.2.51:554/stream1"
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [detect, record]
    detect:
      enabled: true
      fps: 5
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [detect, record]
    detect:
      enabled: true
      fps: 5
"#,
        db_path.display(),
        cold_dir.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir, false)
        .await
        .unwrap();

    // Populate events containing sub_label (PII), license_plate (PII), and label-injection characters
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_fd_1".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!(["porch"]),
            start_time: 1_700_000_000.0,
            end_time: Some(1_700_000_005.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: Some("JaneSensitiveFaceName".to_string()),
            top_score: Some(0.94),
            false_positive: Some(false),
            box_coords: Some(json!([0.1, 0.1, 0.4, 0.8])),
            data: json!({ "recognized_license_plate": "PIIPLATE99" }),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.94),
            stationary: false,
        })
        .unwrap();
    state
        .db
        .set_event_license_plate("ev_fd_1", Some("PIIPLATE99"))
        .unwrap();

    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_by_1".to_string(),
            camera: "backyard".to_string(),
            label: "car\"}\nninjanvr_injected_metric 1\n#".to_string(),
            zones: json!([]),
            start_time: 1_700_000_010.0,
            end_time: Some(1_700_000_015.0),
            has_clip: false,
            has_snapshot: false,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.88),
            false_positive: Some(false),
            box_coords: None,
            data: json!({}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.88),
            stationary: false,
        })
        .unwrap();

    // Create admin token, metrics-scoped token, and camera-scoped viewer user
    let (_, raw_admin_tok) = state
        .db
        .create_api_token("ops_admin", "full", None)
        .unwrap();
    let (_, raw_metrics_tok) = state
        .db
        .create_api_token("tls_prom_scraper", "metrics", None)
        .unwrap();
    let (_, raw_readonly_tok) = state
        .db
        .create_api_token("ro_observer", "read_only", None)
        .unwrap();
    state
        .db
        .create_user_with_cameras(
            "front_viewer",
            "ViewerPass#1234",
            "viewer",
            "front_door",
            1000,
        )
        .unwrap();

    // Bind dedicated metrics listener over real TCP (loopback ephemeral port)
    let metrics_app = build_metrics_router(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let metrics_addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, metrics_app).await.unwrap();
    });

    let client = reqwest::Client::new();

    // 1. Dedicated metrics listener serves GET /metrics and GET /api/metrics WITHOUT authentication
    for path in ["/metrics", "/api/metrics"] {
        let resp = client
            .get(format!("http://{metrics_addr}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "dedicated metrics listener must serve {path} without auth"
        );
        assert_eq!(
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/plain; version=0.0.4; charset=utf-8")
        );
        assert_eq!(
            resp.headers()
                .get(reqwest::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        assert!(
            !resp
                .headers()
                .contains_key(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN),
            "dedicated metrics listener must omit CORS headers so cross-origin browsers cannot read telemetry"
        );

        let body = resp.text().await.unwrap();
        // Full instance visibility (both cameras)
        assert!(body.contains("ninjanvr_camera_fps{camera=\"front_door\",role=\"camera\"}"));
        assert!(body.contains("ninjanvr_camera_fps{camera=\"backyard\",role=\"camera\"}"));
        assert!(body.contains("ninjanvr_camera_online{camera=\"front_door\"}"));
        assert!(body.contains("ninjanvr_camera_online{camera=\"backyard\"}"));
        assert!(body.contains("ninjanvr_detector_inferences_total{camera=\"front_door\"}"));
        assert!(body.contains("ninjanvr_detector_inferences_total{camera=\"backyard\"}"));
        assert!(body.contains("ninjanvr_events_total{camera=\"front_door\",label=\"person\"} 1"));
        assert!(body.contains("ninjanvr_storage_used_bytes{mount=\"recordings\"}"));
        assert!(
            body.contains("ninjanvr_storage_used_bytes{mount=\"cold\"}"),
            "cold_dir storage tier must normalize to mount=\"cold\" without duplicating recordings or leaking host path"
        );
        assert!(
            !body.contains("recordings_cold_archive"),
            "raw cold_dir host filesystem path must never leak in unauthenticated OpenMetrics labels"
        );
        assert!(body.contains("ninjanvr_cpu_usage_percent "));
        assert!(body.contains("ninjanvr_memory_usage_bytes "));

        // Strict redaction / zero secret or PII leakage + label escaping
        assert!(!body.contains("SuperSecretRtspPass123"));
        assert!(!body.contains("AnotherSecretPass456"));
        assert!(!body.contains("192.0.2.50"));
        assert!(!body.contains("JaneSensitiveFaceName"));
        assert!(!body.contains("PIIPLATE99"));
        assert!(!body.contains(&raw_admin_tok));
        assert!(!body.contains(&raw_metrics_tok));
        assert!(!body.contains("\nninjanvr_injected_metric 1"));
        assert!(body.contains(r#"label="car\"}\nninjanvr_injected_metric 1\n#""#));
    }

    // 2. Lightweight healthz and version probes work on the dedicated metrics listener
    for health_path in ["/healthz", "/api/healthz"] {
        let resp = client
            .get(format!("http://{metrics_addr}{health_path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "ok\n");
    }
    for ver_path in ["/version", "/api/version"] {
        let resp = client
            .get(format!("http://{metrics_addr}{ver_path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), NINJANVR_VERSION);
    }

    // 3. Dedicated metrics listener strictly isolates surface: NO API, config, auth, media, MCP, or WS routes
    for forbidden_path in [
        "/",
        "/login",
        "/api/config",
        "/api/config/raw",
        "/api/events",
        "/api/stats",
        "/api/users",
        "/api/tokens",
        "/api/mcp",
        "/ws",
        "/clips/front_door-ev1.webp",
        "/recordings/seg.mp4",
        "/exports/exp.mp4",
        "/vod/front_door/start/0/end/10/master.m3u8",
        "/live/mse/api/ws",
        "/share/some_token",
    ] {
        let resp = client
            .get(format!("http://{metrics_addr}{forbidden_path}"))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {raw_admin_tok}"),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND,
            "dedicated metrics listener must never mount NVR route {forbidden_path}"
        );
    }

    let post_metrics = client
        .post(format!("http://{metrics_addr}/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        post_metrics.status(),
        reqwest::StatusCode::METHOD_NOT_ALLOWED
    );

    // Immediate cache invalidation on event insert & delete within the 2s unscoped cache TTL
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_fd_2".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 1_700_000_020.0,
            end_time: Some(1_700_000_025.0),
            has_clip: false,
            has_snapshot: false,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.91),
            false_positive: Some(false),
            box_coords: None,
            data: json!({}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.91),
            stationary: false,
        })
        .unwrap();
    let after_insert = client
        .get(format!("http://{metrics_addr}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        after_insert.contains("ninjanvr_events_total{camera=\"front_door\",label=\"person\"} 2"),
        "inserting an event must invalidate event_counts_cache and unscoped_metrics_cache immediately"
    );
    state.db.delete_event("ev_fd_2").unwrap();
    let after_delete = client
        .get(format!("http://{metrics_addr}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        after_delete.contains("ninjanvr_events_total{camera=\"front_door\",label=\"person\"} 1"),
        "deleting an event must invalidate event_counts_cache and unscoped_metrics_cache immediately"
    );

    server_handle.abort();

    // 4. Application listeners (:5000 / :8971) remain strictly fail-closed by default
    let auth_app = build_router_with_auth(state.clone(), true);
    assert!(is_protected_path("/api/metrics"));
    assert!(is_protected_path("/metrics"));
    assert!(!is_protected_path("/healthz"));
    assert!(!is_protected_path("/api/healthz"));
    assert!(!is_protected_path("/api/version"));

    for protected_metrics_uri in ["/api/metrics", "/metrics"] {
        let unauth = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(protected_metrics_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            unauth.status(),
            StatusCode::UNAUTHORIZED,
            "application router must reject unauthenticated {protected_metrics_uri} with 401"
        );

        let with_metrics_tok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(protected_metrics_uri)
                    .header(header::AUTHORIZATION, format!("Bearer {raw_metrics_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(with_metrics_tok.status(), StatusCode::OK);

        // read_only token strictly encompasses metrics scope on both /metrics and /api/metrics
        let with_readonly_tok = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(protected_metrics_uri)
                    .header(header::AUTHORIZATION, format!("Bearer {raw_readonly_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            with_readonly_tok.status(),
            StatusCode::OK,
            "read_only token must be allowed to GET {protected_metrics_uri}"
        );
    }

    // Camera-scoped JWT caller on application router sees only front_door, never backyard
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let viewer_jwt = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &JwtClaims {
            sub: "front_viewer".to_string(),
            role: "viewer".to_string(),
            allowed_cameras: Some("front_door".to_string()),
            iat: now,
            exp: now + 3600,
        },
        &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .unwrap();
    let scoped_resp = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/metrics")
                .header(header::AUTHORIZATION, format!("Bearer {viewer_jwt}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(scoped_resp.status(), StatusCode::OK);
    let scoped_body = String::from_utf8(
        to_bytes(scoped_resp.into_body(), 65536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(scoped_body.contains("camera=\"front_door\""));
    assert!(!scoped_body.contains("camera=\"backyard\""));

    // Unauthenticated liveness probe /healthz stays 200 OK on fail-closed application router
    let app_healthz = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(app_healthz.status(), StatusCode::OK);
    let healthz_bytes = to_bytes(app_healthz.into_body(), 1024).await.unwrap();
    assert_eq!(&healthz_bytes[..], b"ok\n");

    // 5. SSRF and secret-env-var guards block DEFAULT_METRICS_PORT (9091), custom active metrics_port,
    //    camera WebUI proxy, GenAI validate, federation proxy, and NINJANVR_* internal env vars
    assert_eq!(DEFAULT_METRICS_PORT, 9091);
    assert!(validate_genai_base_url("http://198.51.100.20:9091/v1").is_err());
    assert!(validate_genai_base_url("http://198.51.100.20:8555/v1").is_err());
    assert!(is_disallowed_federation_port(DEFAULT_METRICS_PORT));
    assert!(!is_safe_secret_env_var_name("NINJANVR_METRICS_PORT"));
    assert!(!is_safe_secret_env_var_name("NINJANVR_METRICS_BIND"));
    assert!(!is_safe_secret_env_var_name(
        "NINJANVR_INSECURE_HTTP_NO_AUTH"
    ));

    state.set_active_listener_ports(5000, 8971, 9195);
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().webui_url =
            Some("http://198.51.100.25:9091".to_string());
        assert_eq!(
            cfg.resolve_camera_webui_base_url("front_door").as_deref(),
            Some("http://192.0.2.50"),
            "camera webui_url targeting port 9091 must be rejected and never returned"
        );
        cfg.cameras.insert(
            "webui_only_9091".to_string(),
            ninjanvr_config::CameraConfig {
                webui_url: Some("http://198.51.100.25:9091".to_string()),
                ..Default::default()
            },
        );
        assert!(
            cfg.resolve_camera_webui_base_url("webui_only_9091")
                .is_none(),
            "camera with only port-9091 webui_url and no RTSP upstream must resolve to None"
        );
        cfg.cameras.get_mut("front_door").unwrap().webui_url =
            Some("http://198.51.100.25:9195".to_string());
        cfg.peers.insert(
            "custom_metrics_peer".to_string(),
            ninjanvr_config::PeerNodeConfig {
                enabled: true,
                url: "http://198.51.100.25:9195".to_string(),
                token_env: String::new(),
            },
        );
    }

    let cam_proxy_custom = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/camera-proxy/front_door/metrics")
                .header(header::AUTHORIZATION, format!("Bearer {raw_admin_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        cam_proxy_custom.status(),
        StatusCode::BAD_REQUEST,
        "camera-proxy must block active custom metrics listener port 9195"
    );

    let genai_custom = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/genai/validate")
                .header(header::AUTHORIZATION, format!("Bearer {raw_admin_tok}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "provider": "openai",
                        "api_key": "sk-test",
                        "base_url": "http://127.0.0.1:9195/v1"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        genai_custom.status(),
        StatusCode::BAD_REQUEST,
        "/api/genai/validate must block active custom metrics listener port 9195"
    );

    let fed_custom = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/federation/custom_metrics_peer/metrics")
                .header(header::AUTHORIZATION, format!("Bearer {raw_admin_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        fed_custom.status(),
        StatusCode::BAD_REQUEST,
        "/api/federation/* must block active custom metrics listener port 9195"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
