use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ws::{Message as AxumWsMsg, WebSocketUpgrade};
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use ninjanvr_server::{
    JwtClaims, ShareTokenPayload, build_router_with_auth, ensure_path_within_media_dir,
    extract_onvif_soap_fault, filter_ws_message_for_allowed_cameras, is_safe_onvif_endpoint_url,
    new_app_state, normalize_privacy_mask_value_for_resolution, parse_onvif_presets_xml,
    parse_onvif_status_xml, resolve_onvif_ptz_endpoint, sign_share_token,
};
use serde_json::{Value, json};
use tower::ServiceExt;

fn mint_test_jwt(secret: &str, sub: &str, role: &str, allowed_cameras: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &JwtClaims {
            sub: sub.to_string(),
            role: role.to_string(),
            allowed_cameras: Some(allowed_cameras.to_string()),
            iat: now,
            exp: now + 3600,
        },
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

#[tokio::test]
async fn share_links_and_media_containment_regression() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-p3c-share-test-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let media_dir = tmp_dir.join("media");
    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    std::fs::create_dir_all(&exports_dir).unwrap();
    std::fs::create_dir_all(&clips_dir).unwrap();

    // 1. ensure_path_within_media_dir rejects media_dir root, dotfiles, and symlinks to dotfiles
    let valid_mp4 = exports_dir.join("front_door_100_110.mp4");
    std::fs::write(&valid_mp4, b"valid").unwrap();
    assert!(ensure_path_within_media_dir(&media_dir, &valid_mp4).is_some());
    assert!(
        ensure_path_within_media_dir(&media_dir, &media_dir).is_none(),
        "media_dir root itself must not be accepted as a file path"
    );

    let dot_secret = media_dir.join(".jwt_secret");
    std::fs::write(&dot_secret, b"super-secret-jwt-key").unwrap();
    assert!(
        ensure_path_within_media_dir(&media_dir, &dot_secret).is_none(),
        "dotfile at media_dir root must be rejected"
    );

    let dot_sidecar = exports_dir.join(".share_redacted_ev1.mask_hash");
    std::fs::write(&dot_sidecar, b"hash").unwrap();
    assert!(
        ensure_path_within_media_dir(&media_dir, &dot_sidecar).is_none(),
        "dotfile inside exports/ must be rejected"
    );

    #[cfg(unix)]
    {
        let symlink_to_dot = exports_dir.join("innocent_export.mp4");
        std::os::unix::fs::symlink(&dot_secret, &symlink_to_dot).unwrap();
        assert!(
            ensure_path_within_media_dir(&media_dir, &symlink_to_dot).is_none(),
            "symlink resolving to a dotfile inside media_dir must be rejected"
        );
    }

    // 2. normalize_privacy_mask_value_for_resolution normalizes string, flat array, pair array, and object masks
    let str_mask = json!("0,0,320,240");
    let norm_str = normalize_privacy_mask_value_for_resolution(&str_mask, 640, 480);
    let vf_str = ninjanvr_media::build_privacy_drawbox_filter(Some(&norm_str)).unwrap();
    assert!(
        vf_str.contains("iw*0.000") && vf_str.contains("iw*0.500") && vf_str.contains("ih*0.500"),
        "expected proportional iw/ih drawbox filter, got: {vf_str}"
    );

    let flat_arr_mask = json!([320, 240, 640, 480]);
    let norm_flat = normalize_privacy_mask_value_for_resolution(&flat_arr_mask, 640, 480);
    let vf_flat = ninjanvr_media::build_privacy_drawbox_filter(Some(&norm_flat)).unwrap();
    assert!(
        vf_flat.contains("iw*0.500") && vf_flat.contains("ih*0.500"),
        "expected proportional iw/ih drawbox filter for flat array, got: {vf_flat}"
    );

    let pair_arr_mask = json!([[0, 0], [320, 0], [320, 240], [0, 240]]);
    let norm_pairs = normalize_privacy_mask_value_for_resolution(&pair_arr_mask, 640, 480);
    let vf_pairs = ninjanvr_media::build_privacy_drawbox_filter(Some(&norm_pairs)).unwrap();
    assert!(
        vf_pairs.contains("iw*0.500") && vf_pairs.contains("ih*0.500"),
        "expected proportional iw/ih drawbox filter for pair array, got: {vf_pairs}"
    );

    let obj_mask = json!({
        "mode": "pixelate",
        "block_size": 8,
        "zones": [[[320, 240], [640, 240], [640, 480], [320, 480]]]
    });
    let norm_obj = normalize_privacy_mask_value_for_resolution(&obj_mask, 640, 480);
    let vf_obj = ninjanvr_media::build_privacy_drawbox_filter(Some(&norm_obj)).unwrap();
    assert!(
        vf_obj.contains("iw*0.500") && vf_obj.contains("ih*0.500"),
        "expected proportional iw/ih drawbox filter for nested object mask, got: {vf_obj}"
    );

    // 3. Trimmed camera names in review/export share links do not trigger false camera mismatch
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();
    std::fs::write(
        &cfg_path,
        format!(
            "version: 0.18-0\ndatabase:\n  path: {}\ncameras:\n  front_door:\n    enabled: true\n",
            db_path.display()
        ),
    )
    .unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let app = build_router_with_auth(state.clone(), true);

    let exp_path = exports_dir.join("exp_whitespace_cam.mp4");
    std::fs::write(&exp_path, b"\x00\x00\x00\x14ftypisom\x00\x00\x02\x00isom").unwrap();
    state
        .db
        .insert_export(&ninjanvr_db::ExportRow {
            id: "exp_whitespace_cam".to_string(),
            camera: "  front_door  ".to_string(),
            name: "Whitespace Cam Export".to_string(),
            date: 1700000000.0,
            video_path: "/media/ninjanvr/exports/exp_whitespace_cam.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_whitespace_cam.webp".to_string(),
            in_progress: false,
        })
        .unwrap();

    let now = 1900000000u64;
    state
        .db
        .insert_share_link_with_options(
            "shr_ws_1",
            "export",
            "exp_whitespace_cam",
            Some("front_door"),
            "admin",
            1700000000.0,
            now as f64,
            None,
        )
        .unwrap();
    let tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_whitespace_cam".to_string(),
            exp: now,
            cam: Some("front_door".to_string()),
            jti: Some("shr_ws_1".to_string()),
        },
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn cases_exports_and_previews_scoping_regression() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-p3c-cases-test-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let media_dir = tmp_dir.join("media");
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
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
  front:
    enabled: true
  front-door:
    enabled: true
  backyard:
    enabled: true
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("scoped_front", "pass1234", "operator", "front", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("scoped_fd", "pass1234", "operator", "front-door", 1000)
        .unwrap();

    let app = build_router_with_auth(state.clone(), true);
    let front_bearer = format!(
        "Bearer {}",
        mint_test_jwt(&state.jwt_secret, "scoped_front", "operator", "front")
    );
    let fd_bearer = format!(
        "Bearer {}",
        mint_test_jwt(&state.jwt_secret, "scoped_fd", "operator", "front-door")
    );

    // 1. POST /api/cases rejects unsafe case_id (path traversal) with 400 BAD_REQUEST
    let bad_id_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/cases")
                .header(header::AUTHORIZATION, &front_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "id": "../traversal_case",
                        "name": "Bad Case"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_id_resp.status(), StatusCode::BAD_REQUEST);

    // 2. Case referencing a review segment on forbidden camera "backyard" or a deleted "backyard-*" event
    //    is hidden from "front" user and cannot be created/overwritten by "front" user
    state
        .db
        .insert_review_segment(&ninjanvr_db::ReviewItemDto {
            id: "rev_by_secret".to_string(),
            camera: "backyard".to_string(),
            start_time: 1000.0,
            end_time: Some(1015.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-backyard-rev_by_secret.webp"
                .to_string(),
            data: json!({ "objects": ["person"] }),
            has_been_reviewed: false,
        })
        .unwrap();
    state
        .db
        .create_case(
            "case_rev_by",
            "Backyard Review Case",
            None,
            &["rev_by_secret".to_string()],
            &[],
        )
        .unwrap();
    state
        .db
        .create_case(
            "case_deleted_by",
            "Deleted Backyard Event Case",
            None,
            &["backyard-deleted_999".to_string()],
            &[],
        )
        .unwrap();

    let get_cases_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/cases")
                .header(header::AUTHORIZATION, &front_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_cases_resp.status(), StatusCode::OK);
    let cases_json: Vec<Value> =
        serde_json::from_slice(&to_bytes(get_cases_resp.into_body(), 16384).await.unwrap())
            .unwrap();
    assert!(
        cases_json
            .iter()
            .all(|c| c["id"] != "case_rev_by" && c["id"] != "case_deleted_by"),
        "scoped user must not see cases referencing review segments or deleted events on forbidden cameras"
    );

    for forbidden_case in [
        json!({
            "id": "case_try_rev",
            "name": "Try Attach Forbidden Review",
            "event_ids": ["rev_by_secret"]
        }),
        json!({
            "id": "case_try_del_exp",
            "name": "Try Attach Forbidden Deleted Export",
            "export_ids": ["backyard_1000_1010"]
        }),
    ] {
        let post_forbid = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/cases")
                    .header(header::AUTHORIZATION, &front_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(forbidden_case.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(post_forbid.status(), StatusCode::FORBIDDEN);
    }

    // 3. Preview thumbnail rejects missing "preview_" prefix (no "sala" fallback!)
    //    and accurately scopes hyphenated camera "front-door" vs "front"
    let no_prefix_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/preview/arbitrary_non_preview/thumbnail.webp")
                .header(header::AUTHORIZATION, &front_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(no_prefix_resp.status(), StatusCode::BAD_REQUEST);

    let hyphen_forbid_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/preview/preview_front-door-1700000000/thumbnail.webp")
                .header(header::AUTHORIZATION, &front_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        hyphen_forbid_resp.status(),
        StatusCode::FORBIDDEN,
        "user permitted only for 'front' must be forbidden from 'front-door' preview thumbnail"
    );

    let hyphen_allow_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/preview/preview_front-door-1700000000/thumbnail.webp")
                .header(header::AUTHORIZATION, &fd_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(hyphen_allow_resp.status(), StatusCode::OK);

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn onvif_ptz_ssrf_bounds_xml_and_rbac_regression() {
    // 1. SSRF endpoint validation
    for blocked in [
        "http://127.0.0.1:5000/onvif/ptz_service",
        "http://127.0.0.1:8971/onvif/ptz_service",
        "http://127.0.0.1:1984/onvif/ptz_service",
        "http://127.0.0.1:8554/onvif/ptz_service",
        "http://127.0.0.1:8555/onvif/ptz_service",
        "http://0.0.0.0:80/onvif/ptz_service",
        "http://[::]:80/onvif/ptz_service",
        "http://169.254.169.254/latest/meta-data/",
        "http://[::ffff:169.254.169.254]:80/onvif/ptz_service",
        "http://[fe80::1]:80/onvif/ptz_service",
        "http://metadata.google.internal/computeMetadata/v1/",
        "http://sub.metadata.google.internal/onvif/ptz_service",
        "http://255.255.255.255:80/onvif/ptz_service",
        "http://224.0.0.1:80/onvif/ptz_service",
        "http://192.0.2.10:80/onvif/ptz_service?query=1",
        "http://192.0.2.10:80/onvif/ptz_service#frag",
    ] {
        assert!(
            !is_safe_onvif_endpoint_url(blocked),
            "expected ONVIF URL to be blocked: {blocked}"
        );
    }
    assert!(is_safe_onvif_endpoint_url(
        "http://192.0.2.25:8080/onvif/ptz_service"
    ));

    let mut bad_onvif = ninjanvr_config::OnvifCameraConfig {
        host: Some("169.254.169.254".to_string()),
        port: 80,
        ..Default::default()
    };
    assert_eq!(resolve_onvif_ptz_endpoint(&bad_onvif), None);
    bad_onvif.host = Some("127.0.0.1".to_string());
    bad_onvif.port = 1984;
    assert_eq!(resolve_onvif_ptz_endpoint(&bad_onvif), None);

    // 2. XML parser accuracy & bounds
    let presets_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body>
    <tptz:GetPresetsResponse>
      <tptz:Preset token="gate_1">
        <tt:Name>Front Gate</tt:Name>
        <tt:Extension><ext:Preset>nested</ext:Preset></tt:Extension>
      </tptz:Preset>
    </tptz:GetPresetsResponse>
  </s:Body>
</s:Envelope>"#;
    let parsed_presets = parse_onvif_presets_xml(presets_xml);
    assert_eq!(parsed_presets.len(), 1);
    assert_eq!(parsed_presets[0].token, "gate_1");
    assert_eq!(parsed_presets[0].name, "Front Gate");

    // Non-self-closing empty <tt:PanTilt x="..." y="..."></tt:PanTilt> before <tt:MoveStatus><tt:PanTilt>MOVING</tt:PanTilt>
    let status_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body>
    <tptz:GetStatusResponse>
      <tptz:PTZStatus>
        <tt:Position>
          <tt:PanTilt x="+0.25" y="-0.50"></tt:PanTilt>
          <tt:Zoom x="0.75"></tt:Zoom>
        </tt:Position>
        <tt:MoveStatus>
          <tt:PanTilt>MOVING</tt:PanTilt>
        </tt:MoveStatus>
      </tptz:PTZStatus>
    </tptz:GetStatusResponse>
  </s:Body>
</s:Envelope>"#;
    let status = parse_onvif_status_xml(status_xml);
    assert!((status.pan - 0.25).abs() < 1e-4);
    assert!((status.tilt - (-0.50)).abs() < 1e-4);
    assert!((status.zoom - 0.75).abs() < 1e-4);
    assert_eq!(status.move_status, "MOVING");

    let fault_with_creds = r#"<s:Fault><s:Reason><s:Text>Failed upstream rtsp://admin:topsecret99@192.0.2.10:554/stream</s:Text></s:Reason></s:Fault>"#;
    let fault_msg = extract_onvif_soap_fault(fault_with_creds).unwrap();
    assert!(!fault_msg.contains("topsecret99"));
    assert!(fault_msg.contains("rtsp://*:*@192.0.2.10:554/stream"));

    // 3. Mock ONVIF server testing redirect rejection, oversized body rejection, and RBAC/camera scoping
    let mode = Arc::new(std::sync::Mutex::new("redirect".to_string()));
    let mode_clone = mode.clone();
    let mock_onvif = Router::new().route(
        "/onvif/ptz_service",
        post(move || {
            let m = mode_clone.lock().unwrap().clone();
            async move {
                if m == "redirect" {
                    (
                        StatusCode::TEMPORARY_REDIRECT,
                        [(header::LOCATION, "http://169.254.169.254/latest/meta-data/")],
                        String::new(),
                    )
                        .into_response()
                } else {
                    // 300 KiB > 256 KiB cap
                    let huge = "A".repeat(300 * 1024);
                    (StatusCode::OK, huge).into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, mock_onvif).await;
    });

    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-p3c-ptz-test-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    std::fs::write(
        &cfg_path,
        format!(
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
    onvif:
      host: "127.0.0.1"
      port: {}
  backyard:
    enabled: true
    onvif:
      host: "127.0.0.1"
      port: {}
"#,
            db_path.display(),
            addr.port(),
            addr.port()
        ),
    )
    .unwrap();

    let state = new_app_state(&cfg_path, web_dir, tmp_dir.join("media"), false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("scoped_fd", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    let app = build_router_with_auth(state.clone(), true);
    let scoped_bearer = format!(
        "Bearer {}",
        mint_test_jwt(&state.jwt_secret, "scoped_fd", "operator", "front_door")
    );
    let viewer_bearer = format!(
        "Bearer {}",
        mint_test_jwt(&state.jwt_secret, "viewer_u", "viewer", "*")
    );

    // Scoped operator cannot access backyard PTZ
    let forbid_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/backyard/ptz/move")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"action":"left"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(forbid_resp.status(), StatusCode::FORBIDDEN);

    // Viewer cannot mutate PTZ
    let viewer_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/ptz/move")
                .header(header::AUTHORIZATION, &viewer_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"action":"left"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(viewer_resp.status(), StatusCode::FORBIDDEN);

    // Redirect response from ONVIF endpoint is rejected with 502 Bad Gateway
    let redirect_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/ptz/move")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"action":"left"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(redirect_resp.status(), StatusCode::BAD_GATEWAY);

    // Oversized response (> 256 KiB) is rejected with 502 Bad Gateway
    *mode.lock().unwrap() = "oversized".to_string();
    let oversized_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/ptz/move")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"action":"left"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(oversized_resp.status(), StatusCode::BAD_GATEWAY);

    srv_handle.abort();
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn latest_frame_privacy_ws_stats_webrtc_and_vod_regression() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-p3c-media-ws-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    // Configure front_door with a pixel-space privacy mask on the bottom-right quadrant of 640x480
    std::fs::write(
        &cfg_path,
        format!(
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
    detect:
      width: 640
      height: 480
      fps: 5
    privacy_mask:
      - [[320, 240], [640, 240], [640, 480], [320, 480]]
  backyard:
    enabled: true
"#,
            db_path.display()
        ),
    )
    .unwrap();

    let mut state = new_app_state(&cfg_path, web_dir, tmp_dir.join("media"), false)
        .await
        .unwrap();

    // Mock upstream go2rtc WS for /live/webrtc/api/ws test
    let mock_go2rtc = Router::new().route(
        "/api/ws",
        get(|ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|mut socket| async move {
                if let Some(Ok(AxumWsMsg::Text(t))) = socket.next().await {
                    let reply = format!(
                        r#"{{"type":"webrtc/answer","echo":{},"url":"rtsp://admin:secretpass@192.0.2.55:554/live"}}"#,
                        serde_json::to_string(t.as_str()).unwrap()
                    );
                    let _ = socket.send(AxumWsMsg::Text(reply.into())).await;
                }
            })
        }),
    );
    let go2rtc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let go2rtc_addr = go2rtc_listener.local_addr().unwrap();
    let go2rtc_handle = tokio::spawn(async move {
        let _ = axum::serve(go2rtc_listener, mock_go2rtc).await;
    });
    state.go2rtc_ws_base = format!("ws://{go2rtc_addr}");
    state.go2rtc_http_base = format!("http://{go2rtc_addr}");

    let app = build_router_with_auth(state.clone(), true);
    let admin_bearer = format!(
        "Bearer {}",
        mint_test_jwt(&state.jwt_secret, "admin", "admin", "*")
    );

    // 1. Seed a bright YuvFrame (Y=220, U=128, V=128, 640x480) and request downscaled ?height=180
    //    Verify the pixel-space privacy mask [320..640, 240..480] is normalized and redacts the
    //    bottom-right quadrant of the downscaled 240x180 image!
    let w = 640usize;
    let h = 480usize;
    let mut yuv_data = vec![220u8; w * h];
    yuv_data.extend(std::iter::repeat_n(128u8, (w / 2) * (h / 2) * 2));
    state.latest_yuv_frames.write().await.insert(
        "front_door".to_string(),
        ninjanvr_media::YuvFrame {
            camera: "front_door".into(),
            width: w as u32,
            height: h as u32,
            timestamp: 1700000123.0,
            data: Arc::new(yuv_data),
        },
    );

    let frame_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/latest.webp?height=180")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(frame_resp.status(), StatusCode::OK);
    let frame_bytes = to_bytes(frame_resp.into_body(), 256 * 1024).await.unwrap();
    let decoded = image::load_from_memory(&frame_bytes).unwrap().to_rgb8();
    assert_eq!(decoded.height(), 180);
    // Top-left (unmasked) pixel should be bright (> 180), bottom-right (masked) pixel should be dark (< 30)
    let tl = decoded.get_pixel(20, 20);
    let br = decoded.get_pixel(decoded.width() - 20, decoded.height() - 20);
    assert!(
        tl[0] > 180,
        "unmasked top-left pixel should be bright: {tl:?}"
    );
    assert!(
        br[0] < 30 && br[1] < 30 && br[2] < 30,
        "masked bottom-right pixel on downscaled ?height=180 image must be redacted: {br:?}"
    );

    // 2. Malformed non-empty privacy_mask must FAIL CLOSED (return dark placeholder instead of unredacted frame)
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask = json!(["invalid_polygon_spec"]);
    }
    let fail_closed_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/latest.jpg")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fail_closed_resp.status(), StatusCode::OK);
    let fc_bytes = to_bytes(fail_closed_resp.into_body(), 256 * 1024)
        .await
        .unwrap();
    let fc_img = image::load_from_memory(&fc_bytes).unwrap().to_rgb8();
    let fc_px = fc_img.get_pixel(20, 20);
    assert!(
        fc_px[0] < 40,
        "malformed active privacy_mask must fail closed to dark placeholder, got: {fc_px:?}"
    );

    // 3. POST /api/{camera}/state rejects unconfigured cameras with 404 NOT_FOUND
    let unconf_state_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/nonexistent_camera_xyz/state")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"record":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unconf_state_resp.status(), StatusCode::NOT_FOUND);
    assert!(
        !state
            .activity_ledger
            .read()
            .await
            .contains_key("nonexistent_camera_xyz")
    );

    // 4. filter_ws_message_for_allowed_cameras filters "stats" payloads for scoped users
    let raw_stats_ws = json!({
        "topic": "stats",
        "payload": {
            "cameras": {
                "front_door": { "camera_fps": 15.0 },
                "backyard": { "camera_fps": 15.0 }
            },
            "service": {
                "version": "2.1.9",
                "camera_storage": {
                    "front_door": { "usage_mb": 10.0 },
                    "backyard": { "usage_mb": 20.0 }
                }
            }
        }
    })
    .to_string();
    let allowed = vec!["front_door".to_string()];
    let filtered_ws = filter_ws_message_for_allowed_cameras(&raw_stats_ws, Some(&allowed)).unwrap();
    let filtered_val: Value = serde_json::from_str(&filtered_ws).unwrap();
    assert!(
        filtered_val["payload"]["cameras"]
            .get("front_door")
            .is_some()
    );
    assert!(
        filtered_val["payload"]["cameras"].get("backyard").is_none(),
        "/ws stats broadcast must strip forbidden cameras"
    );
    assert!(
        filtered_val["payload"]["service"]["camera_storage"]
            .get("backyard")
            .is_none(),
        "/ws stats broadcast must strip forbidden camera_storage entries"
    );

    // 5. /live/webrtc/api/ws?src=front_door relays WebRTC signaling to go2rtc and scrubs RTSP credentials
    let srv_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let srv_addr = srv_listener.local_addr().unwrap();
    let srv_task = tokio::spawn(async move {
        let _ = axum::serve(srv_listener, app).await;
    });

    let mut ws_req =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(format!(
            "ws://{srv_addr}/live/webrtc/api/ws?src=front_door"
        ))
        .unwrap();
    ws_req.headers_mut().insert(
        header::AUTHORIZATION,
        header::HeaderValue::from_str(&admin_bearer).unwrap(),
    );
    let (mut webrtc_ws, _) = tokio_tungstenite::connect_async(ws_req).await.unwrap();
    webrtc_ws
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"webrtc/offer","value":"v=0"}"#.into(),
        ))
        .await
        .unwrap();
    let ws_reply = tokio::time::timeout(Duration::from_secs(2), webrtc_ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let reply_text = ws_reply.into_text().unwrap();
    assert!(reply_text.contains("webrtc/answer"));
    assert!(
        !reply_text.contains("secretpass"),
        "WebRTC WS relay must scrub RTSP credentials: {reply_text}"
    );
    assert!(reply_text.contains("rtsp://*:*@192.0.2.55:554/live"));

    srv_task.abort();
    go2rtc_handle.abort();
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn cycle2_share_export_onvif_ws_cswsh_and_vod_regression() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-p3c-cycle2-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let media_dir = tmp_dir.join("media");
    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    let review_clips_dir = clips_dir.join("review");
    let rec_dir = media_dir.join("recordings/2026-11-05/12/front_door");
    std::fs::create_dir_all(&exports_dir).unwrap();
    std::fs::create_dir_all(&review_clips_dir).unwrap();
    std::fs::create_dir_all(&rec_dir).unwrap();

    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    std::fs::write(
        &cfg_path,
        format!(
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
    detect:
      width: 640
      height: 480
      fps: 5
  backyard:
    enabled: true
    detect:
      width: 640
      height: 480
      fps: 5
"#,
            db_path.display()
        ),
    )
    .unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("scoped_fd", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    let app = build_router_with_auth(state.clone(), true);

    let admin_jwt = mint_test_jwt(&state.jwt_secret, "admin", "admin", "*");
    let admin_bearer = format!("Bearer {admin_jwt}");
    let admin_cookie = format!("ninjanvr_token={admin_jwt}");
    let fd_jwt = mint_test_jwt(&state.jwt_secret, "scoped_fd", "operator", "front_door");
    let fd_bearer = format!("Bearer {fd_jwt}");
    let viewer_bearer = format!(
        "Bearer {}",
        mint_test_jwt(&state.jwt_secret, "viewer_u", "viewer", "*")
    );

    // 1. [P3C-C2-01] Cases: empty admin case shell & deleted `rev_backyard_*` / `exp_backyard_*` cases
    //    are hidden from scoped user and cannot be overwritten by scoped user
    state
        .db
        .create_case(
            "case_admin_empty_shell",
            "Confidential Shell",
            Some("Executive notes before attaching clips"),
            &[],
            &[],
        )
        .unwrap();
    state
        .db
        .create_case(
            "case_deleted_rev_by",
            "Deleted Backyard Review",
            None,
            &["rev_backyard_999".to_string()],
            &[],
        )
        .unwrap();
    state
        .db
        .create_case(
            "case_deleted_exp_by",
            "Deleted Backyard Export",
            None,
            &[],
            &["exp_backyard_999".to_string()],
        )
        .unwrap();

    let cases_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/cases")
                .header(header::AUTHORIZATION, &fd_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cases_resp.status(), StatusCode::OK);
    let visible_cases: Vec<Value> =
        serde_json::from_slice(&to_bytes(cases_resp.into_body(), 16384).await.unwrap()).unwrap();
    assert!(
        visible_cases.iter().all(|c| {
            c["id"] != "case_admin_empty_shell"
                && c["id"] != "case_deleted_rev_by"
                && c["id"] != "case_deleted_exp_by"
        }),
        "scoped user must not see empty admin case shells or deleted backyard cases: {visible_cases:?}"
    );

    // Scoped operator cannot overwrite `case_admin_empty_shell` or attach `rev_backyard_999`
    for forbidden_case in [
        json!({
            "id": "case_admin_empty_shell",
            "name": "Clobber Admin Shell",
            "event_ids": ["front_door_ev_1"]
        }),
        json!({
            "id": "case_try_del_rev_by",
            "name": "Try Deleted Backyard Review",
            "event_ids": ["rev_backyard_999"]
        }),
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/cases")
                    .header(header::AUTHORIZATION, &fd_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(forbidden_case.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    // Unsafe event_ids element in POST /api/cases is rejected with 400 BAD_REQUEST
    let bad_ev_id_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/cases")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "id": "case_bad_ev_elem",
                        "name": "Bad Event ID",
                        "event_ids": ["../outside"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_ev_id_resp.status(), StatusCode::BAD_REQUEST);

    // 2. [P3C-C2-02] Privacy mask normalization for custom object keys + deep recursion bound + disk export fallback
    let custom_obj_mask = json!({
        "mode": "pixelate",
        "block_size": 8,
        "door_zone": "0,0,320,240"
    });
    let norm_custom = normalize_privacy_mask_value_for_resolution(&custom_obj_mask, 640, 480);
    let vf_custom = ninjanvr_media::build_privacy_drawbox_filter(Some(&norm_custom)).unwrap();
    assert!(vf_custom.contains("iw*0.500") && vf_custom.contains("ih*0.500"));
    let poly_custom = ninjanvr_vision::PolygonMask::from_config_value(&norm_custom, 640, 480);
    assert!(!poly_custom.is_empty());

    let mut deep_mask = json!("0,0,320,240");
    for _ in 0..24 {
        deep_mask = json!([deep_mask]);
    }
    let norm_deep = normalize_privacy_mask_value_for_resolution(&deep_mask, 640, 480);
    assert!(
        ninjanvr_media::build_privacy_drawbox_filter(Some(&norm_deep)).is_none(),
        "excessively nested privacy mask (> 16 levels) must fail closed"
    );

    // Disk-only export with single-underscore camera prefix (`backyard_manual.mp4`) resolves to `backyard`
    // and rejects a share token claiming `cam: Some("front_door")`
    std::fs::write(
        exports_dir.join("backyard_manual.mp4"),
        b"\x00\x00\x00\x14ftypisom\x00\x00\x02\x00isom",
    )
    .unwrap();
    let now = 1900000000u64;
    state
        .db
        .insert_share_link_with_options(
            "shr_c2_disk_mismatch",
            "export",
            "backyard_manual",
            Some("front_door"),
            "admin",
            1700000000.0,
            now as f64,
            None,
        )
        .unwrap();
    let mismatch_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "backyard_manual".to_string(),
            exp: now,
            cam: Some("front_door".to_string()),
            jti: Some("shr_c2_disk_mismatch".to_string()),
        },
    );
    for uri in [
        format!("/share/{mismatch_tok}"),
        format!("/share/{mismatch_tok}/video.mp4"),
    ] {
        let r = app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    // 3. [P3C-C2-03] DELETE /api/export/{id} & POST /api/exports/delete never unlink recordings/ or clips/review/
    let keep_recording = rec_dir.join("00.00.mp4");
    let keep_rev_thumb = review_clips_dir.join("thumb-front_door-rev1.webp");
    let keep_latest = clips_dir.join("latest_front_door.jpg");
    std::fs::write(&keep_recording, b"RAW_RECORDING_EVIDENCE").unwrap();
    std::fs::write(&keep_rev_thumb, b"REVIEW_THUMB_EVIDENCE").unwrap();
    std::fs::write(&keep_latest, b"LATEST_SNAPSHOT").unwrap();

    state
        .db
        .insert_export(&ninjanvr_db::ExportRow {
            id: "exp_cross_dir_1".to_string(),
            camera: "front_door".to_string(),
            name: "Cross Dir 1".to_string(),
            date: 1700000000.0,
            video_path: "/media/ninjanvr/recordings/2026-11-05/12/front_door/00.00.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-front_door-rev1.webp".to_string(),
            in_progress: false,
        })
        .unwrap();
    state
        .db
        .insert_export(&ninjanvr_db::ExportRow {
            id: "exp_cross_dir_2".to_string(),
            camera: "front_door".to_string(),
            name: "Cross Dir 2".to_string(),
            date: 1700000000.0,
            video_path: "/media/ninjanvr/recordings/2026-11-05/12/front_door/00.00.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/latest_front_door.jpg".to_string(),
            in_progress: false,
        })
        .unwrap();

    let del_single = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/export/exp_cross_dir_1")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_single.status(), StatusCode::OK);

    let del_bulk_operator = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/exports/delete")
                .header(header::AUTHORIZATION, &fd_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ids":["exp_cross_dir_2"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_bulk_operator.status(), StatusCode::FORBIDDEN);

    let del_bulk = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/exports/delete")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ids":["exp_cross_dir_2"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_bulk.status(), StatusCode::OK);
    assert!(
        keep_recording.exists() && keep_rev_thumb.exists() && keep_latest.exists(),
        "export deletion must never unlink files in recordings/, clips/review/, or latest_*.jpg"
    );

    // 4. [P3C-C2-04] ONVIF non-canonical IPv4, trailing-dot metadata, IPv6 IMDS, nested <Preset>, & attr boundaries
    for blocked_onvif in [
        "http://0xa9fea9fe/onvif/ptz_service",
        "http://2852039166/onvif/ptz_service",
        "http://0251.0376.0251.0376/onvif/ptz_service",
        "http://127.1:8080/onvif/ptz_service",
        "http://169.254.43518/onvif/ptz_service",
        "http://metadata.google.internal./computeMetadata/v1/",
        "http://[fd00:ec2::254]/latest/meta-data/",
        "http://100.100.100.200/latest/meta-data/",
        "http://kubernetes.default.svc/onvif/ptz_service",
    ] {
        assert!(
            !is_safe_onvif_endpoint_url(blocked_onvif),
            "expected Cycle-2 ONVIF SSRF URL to be blocked: {blocked_onvif}"
        );
    }
    let ipv6_onvif = ninjanvr_config::OnvifCameraConfig {
        host: Some("2001:db8::25".to_string()),
        port: 8080,
        ..Default::default()
    };
    assert_eq!(
        resolve_onvif_ptz_endpoint(&ipv6_onvif).as_deref(),
        Some("http://[2001:db8::25]:8080/onvif/ptz_service")
    );

    // Nested <ext:Preset> BEFORE <tt:Name> + attribute suffix `sub_token="wrong"` before `token="gate_1"`
    let nested_before_name_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body>
    <tptz:GetPresetsResponse>
      <tptz:Preset sub_token="wrong_tok" token="gate_1">
        <tt:Extension><ext:Preset>nested_before</ext:Preset></tt:Extension>
        <tt:Name>Front Gate</tt:Name>
      </tptz:Preset>
    </tptz:GetPresetsResponse>
  </s:Body>
</s:Envelope>"#;
    let presets = parse_onvif_presets_xml(nested_before_name_xml);
    assert_eq!(presets.len(), 1);
    assert_eq!(presets[0].token, "gate_1");
    assert_eq!(presets[0].name, "Front Gate");

    // `max_x="1.0"` before `x="0.25"` + `<tt:Error>` RTSP credential scrubbing
    let status_attr_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body>
    <tptz:GetStatusResponse>
      <tptz:PTZStatus>
        <tt:Position>
          <tt:PanTilt max_x="1.0" max_y="1.0" x="0.25" y="-0.50"/>
          <tt:Zoom min_x="0.0" x="0.75"/>
        </tt:Position>
        <tt:MoveStatus><tt:PanTilt>IDLE</tt:PanTilt></tt:MoveStatus>
        <tt:Error>Upstream rtsp://admin:ptzsecret99@192.0.2.10:554/stream</tt:Error>
      </tptz:PTZStatus>
    </tptz:GetStatusResponse>
  </s:Body>
</s:Envelope>"#;
    let st = parse_onvif_status_xml(status_attr_xml);
    assert!((st.pan - 0.25).abs() < 1e-4);
    assert!((st.tilt - (-0.50)).abs() < 1e-4);
    assert!((st.zoom - 0.75).abs() < 1e-4);
    let st_err = st.error.unwrap();
    assert!(!st_err.contains("ptzsecret99"));
    assert!(st_err.contains("rtsp://*:*@192.0.2.10:554/stream"));

    // 5. [P3C-C2-05] CSWSH blocking on /ws & /live/mse/api/ws, WS `/before/camera` filtering, VOD bounds, & TLS SANs
    for ws_uri in ["/ws", "/live/mse/api/ws?src=front_door"] {
        let cswsh_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(ws_uri)
                    .header(header::COOKIE, &admin_cookie)
                    .header(header::ORIGIN, "https://evil.example")
                    .header(header::HOST, "nvr.local:5000")
                    .header(header::CONNECTION, "Upgrade")
                    .header(header::UPGRADE, "websocket")
                    .header("sec-websocket-version", "13")
                    .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            cswsh_resp.status(),
            StatusCode::FORBIDDEN,
            "cross-origin cookie WebSocket upgrade on {ws_uri} must be rejected with 403"
        );
    }

    let viewer_prewarm = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/go2rtc/streams/front_door/prewarm")
                .header(header::AUTHORIZATION, &viewer_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(viewer_prewarm.status(), StatusCode::FORBIDDEN);

    let mixed_event_ws = json!({
        "topic": "events",
        "payload": {
            "before": { "camera": "backyard" },
            "after": { "camera": "front_door" }
        }
    })
    .to_string();
    assert!(
        filter_ws_message_for_allowed_cameras(&mixed_event_ws, Some(&["front_door".to_string()]))
            .is_none(),
        "WS event with forbidden /before/camera must be filtered out"
    );

    for bad_vod_uri in [
        "/vod/front_door/start/200/end/100/index-v1-a1.m3u8",
        "/vod/front_door/start/-10/end/100/init-v1-a1.mp4",
        "/vod/front_door/start/200/end/100/seg-1-v1-a1.m4s",
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(bad_vod_uri)
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    assert!(
        ninjanvr_server::generate_self_signed_tls_config_with_sans(&[
            "https://nvr.example.com:8971/path".to_string(),
            "[::1]:8971".to_string(),
            "invalid..hostname..bad".to_string(),
        ])
        .is_ok(),
        "generate_self_signed_tls_config_with_sans must sanitize URLs/ports and skip malformed SANs"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn cycle3_share_privacy_cases_onvif_ws_and_vod_regression() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-p3c-cycle3-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let media_dir = tmp_dir.join("media");
    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    std::fs::create_dir_all(&exports_dir).unwrap();
    std::fs::create_dir_all(&clips_dir).unwrap();

    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    // Configure front_door with a multi-entry privacy_mask array containing both a string polygon
    // (top-left quadrant) and an object with `zones` (bottom-right quadrant) on 640x480
    std::fs::write(
        &cfg_path,
        format!(
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
    detect:
      width: 640
      height: 480
      fps: 5
    privacy_mask:
      - "0,0,160,120"
      - zones:
          - [[320, 240], [640, 240], [640, 480], [320, 480]]
  backyard:
    enabled: true
    detect:
      width: 640
      height: 480
      fps: 5
"#,
            db_path.display()
        ),
    )
    .unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("scoped_fd", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("ws_operator", "pass1234", "operator", "*", 1000)
        .unwrap();
    let app = build_router_with_auth(state.clone(), true);

    let admin_jwt = mint_test_jwt(&state.jwt_secret, "admin", "admin", "*");
    let admin_bearer = format!("Bearer {admin_jwt}");
    let fd_jwt = mint_test_jwt(&state.jwt_secret, "scoped_fd", "operator", "front_door");
    let fd_bearer = format!("Bearer {fd_jwt}");
    let ws_op_jwt = mint_test_jwt(&state.jwt_secret, "ws_operator", "operator", "*");
    let ws_op_bearer = format!("Bearer {ws_op_jwt}");

    // 1. [P3C-C3-01] Multi-polygon array-of-objects mask redacts ALL zones on /api/front_door/latest.webp?height=180
    let w = 640usize;
    let h = 480usize;
    let mut yuv_data = vec![220u8; w * h];
    yuv_data.extend(std::iter::repeat_n(128u8, (w / 2) * (h / 2) * 2));
    state.latest_yuv_frames.write().await.insert(
        "front_door".to_string(),
        ninjanvr_media::YuvFrame {
            camera: "front_door".into(),
            width: w as u32,
            height: h as u32,
            timestamp: 1700000555.0,
            data: Arc::new(yuv_data),
        },
    );

    let frame_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/latest.webp?height=180")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(frame_resp.status(), StatusCode::OK);
    let frame_bytes = to_bytes(frame_resp.into_body(), 256 * 1024).await.unwrap();
    let decoded = image::load_from_memory(&frame_bytes).unwrap().to_rgb8();
    let tl = decoded.get_pixel(10, 10);
    let tr = decoded.get_pixel(decoded.width() - 20, 10);
    let br = decoded.get_pixel(decoded.width() - 20, decoded.height() - 20);
    assert!(
        tl[0] < 30 && br[0] < 30 && tr[0] > 180,
        "both string zone (tl={tl:?}) and nested object zone (br={br:?}) must be redacted while unmasked tr={tr:?} stays bright"
    );

    // Mixed-key object mask (`coordinates` + custom `door_zone`) compiles both drawboxes & polygons
    let mixed_obj_mask = json!({
        "coordinates": "0,0,160,120",
        "door_zone": "320,240,640,480"
    });
    let norm_mixed = normalize_privacy_mask_value_for_resolution(&mixed_obj_mask, 640, 480);
    let vf_mixed = ninjanvr_media::build_privacy_drawbox_filter(Some(&norm_mixed)).unwrap();
    assert_eq!(
        vf_mixed.matches("drawbox=").count(),
        2,
        "mixed-key object mask must compile drawbox filters for both keys: {vf_mixed}"
    );
    let poly_mixed = ninjanvr_vision::PolygonMask::from_config_value(&norm_mixed, 640, 480);
    assert!(poly_mixed.contains_point(50.0, 50.0) && poly_mixed.contains_point(400.0, 300.0));

    // Collinear zero-area polygon alongside valid polygon & boolean/numeric masks fail closed
    for invalid_mask in [
        json!(["0,0,160,120", "0,0,200,200,400,400"]),
        json!(["0,0,160,120", "invalid_poly"]),
        json!(true),
        json!(42),
    ] {
        let norm_inv = normalize_privacy_mask_value_for_resolution(&invalid_mask, 640, 480);
        assert!(
            ninjanvr_media::build_privacy_drawbox_filter(Some(&norm_inv)).is_none(),
            "invalid or partially collinear mask {invalid_mask} must fail closed, got {norm_inv}"
        );
    }

    // Disk-only export `front_door_2026_11_05.mp4` resolves to `front_door` (not `front_door_2026`)
    std::fs::write(
        exports_dir.join("front_door_2026_11_05.mp4"),
        b"\x00\x00\x00\x14ftypisom\x00\x00\x02\x00isom",
    )
    .unwrap();
    let now = 1900000000u64;
    state
        .db
        .insert_share_link_with_options(
            "shr_c3_disk_cam",
            "export",
            "front_door_2026_11_05",
            Some("backyard"),
            "admin",
            1700000000.0,
            now as f64,
            None,
        )
        .unwrap();
    let disk_mismatch_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "front_door_2026_11_05".to_string(),
            exp: now,
            cam: Some("backyard".to_string()),
            jti: Some("shr_c3_disk_cam".to_string()),
        },
    );
    let r_disk_mismatch = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{disk_mismatch_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_disk_mismatch.status(), StatusCode::FORBIDDEN);

    // 2. [P3C-C3-02] Scoped user cannot create empty case shells or attach exports whose video_path belongs to forbidden camera
    state
        .db
        .insert_export(&ninjanvr_db::ExportRow {
            id: "exp_999888".to_string(),
            camera: String::new(),
            name: "Legacy Empty-Camera Backyard Export".to_string(),
            date: 1700000000.0,
            video_path: "/media/ninjanvr/exports/backyard_1700000000_1700000010.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/backyard_1700000000_1700000010.webp".to_string(),
            in_progress: false,
        })
        .unwrap();
    state
        .db
        .create_case(
            "case_legacy_by_exp",
            "Legacy Backyard Export Case",
            None,
            &[],
            &["exp_999888".to_string()],
        )
        .unwrap();

    let get_cases_fd = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/cases")
                .header(header::AUTHORIZATION, &fd_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_cases_fd.status(), StatusCode::OK);
    let fd_cases: Vec<Value> =
        serde_json::from_slice(&to_bytes(get_cases_fd.into_body(), 16384).await.unwrap()).unwrap();
    assert!(
        fd_cases.iter().all(|c| c["id"] != "case_legacy_by_exp"),
        "scoped user must not see cases referencing exports whose video_path belongs to a forbidden camera: {fd_cases:?}"
    );

    for forbidden_case_body in [
        json!({
            "id": "case_fd_empty_try",
            "name": "Empty Scoped Case",
            "event_ids": [],
            "export_ids": []
        }),
        json!({
            "id": "case_fd_legacy_exp_try",
            "name": "Legacy Backyard Export Try",
            "export_ids": ["exp_999888"]
        }),
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/cases")
                    .header(header::AUTHORIZATION, &fd_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(forbidden_case_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    // 3. [P3C-C3-03] ONVIF SSRF `@` authority / IPv4-compat / NAT64, XML comments, Fault precision, & MoveStatus
    for blocked_url in [
        "http://user@169.254.169.254@192.0.2.10:8080/onvif/ptz_service",
        "http://192.0.2.10@169.254.169.254/onvif/ptz_service",
        "http://[::169.254.169.254]:80/onvif/ptz_service",
        "http://[64:ff9b::169.254.169.254]:80/onvif/ptz_service",
    ] {
        assert!(
            !is_safe_onvif_endpoint_url(blocked_url),
            "expected ONVIF SSRF URL to be blocked: {blocked_url}"
        );
    }
    let upper_scheme_onvif = ninjanvr_config::OnvifCameraConfig {
        host: Some("HTTP://192.0.2.25:8080".to_string()),
        port: 8080,
        ..Default::default()
    };
    assert_eq!(
        resolve_onvif_ptz_endpoint(&upper_scheme_onvif).as_deref(),
        Some("HTTP://192.0.2.25:8080/onvif/ptz_service")
    );

    assert_eq!(
        extract_onvif_soap_fault(
            r#"<!-- <s:Fault><faultstring>CommentedFault</faultstring></s:Fault> --><s:Envelope><s:Body><tptz:GetPresetsResponse><tptz:Preset token="p1"><tt:Name>Zone:Faulty</tt:Name><tt:FaultStatus>OK</tt:FaultStatus></tptz:Preset></tptz:GetPresetsResponse></s:Body></s:Envelope>"#
        ),
        None
    );

    let commented_preset_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body>
    <tptz:GetPresetsResponse>
      <!-- <tptz:Preset token="commented_ghost"><tt:Name>Ghost</tt:Name></tptz:Preset> -->
      <tptz:Preset token="real_1"><tt:Name>Real Preset</tt:Name></tptz:Preset>
    </tptz:GetPresetsResponse>
  </s:Body>
</s:Envelope>"#;
    let c3_presets = parse_onvif_presets_xml(commented_preset_xml);
    assert_eq!(c3_presets.len(), 1);
    assert_eq!(c3_presets[0].token, "real_1");

    let status_with_position_children_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body>
    <tptz:GetStatusResponse>
      <tptz:PTZStatus>
        <!-- <tt:PanTilt x="0.99" y="0.99"/> -->
        <tt:Position>
          <tt:PanTilt x="0.25" y="-0.50">
            <tt:Space>http://www.onvif.org/ver10/tptz/PanTiltSpaces/PositionGenericSpace</tt:Space>
          </tt:PanTilt>
          <tt:Zoom x="0.75"/>
        </tt:Position>
        <tt:MoveStatus>
          <tt:PanTilt>IDLE</tt:PanTilt>
          <tt:Zoom>MOVING</tt:Zoom>
        </tt:MoveStatus>
      </tptz:PTZStatus>
    </tptz:GetStatusResponse>
  </s:Body>
</s:Envelope>"#;
    let st_c3 = parse_onvif_status_xml(status_with_position_children_xml);
    assert!((st_c3.pan - 0.25).abs() < 1e-4);
    assert!((st_c3.tilt - (-0.50)).abs() < 1e-4);
    assert_eq!(st_c3.move_status, "MOVING");

    // 4. [P3C-C3-04] WebSocket MSE Hub bypass / `dst=` rejection, top_cam WS filter, live WS role revocation, & VOD bounds
    for (method, bad_stream_uri) in [
        ("GET", "/live/mse/api/ws?src=front_door&src=front_door"),
        ("GET", "/live/mse/api/ws?SRC=front_door"),
        ("GET", "/live/mse/api/ws?%73rc=front_door"),
        ("GET", "/live/mse/api/ws?dst=front_door"),
        ("GET", "/live/webrtc/api/ws?dst=front_door"),
        ("POST", "/api/go2rtc/webrtc?dst=front_door"),
        ("POST", "/api/go2rtc/webrtc?src=front_door&dst=front_door"),
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(bad_stream_uri)
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::BAD_REQUEST,
            "expected 400 BAD_REQUEST for ambiguous/dst stream query {method} {bad_stream_uri}"
        );
    }

    let top_cam_mismatch_ws = json!({
        "topic": "events",
        "payload": {
            "camera": "backyard",
            "after": { "camera": "front_door" }
        }
    })
    .to_string();
    assert!(
        filter_ws_message_for_allowed_cameras(
            &top_cam_mismatch_ws,
            Some(&["front_door".to_string()])
        )
        .is_none(),
        "WS event with forbidden top-level camera must be filtered out"
    );

    for bad_init_uri in [
        "/vod/front_door/start/100/end/200/init-v1-a1.mp4?seg=0",
        "/vod/front_door/start/100/end/200/init-v1-a1.mp4?seg=99999",
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(bad_init_uri)
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    // Live `/ws` role downgrade and camera restriction re-verification
    let srv_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let srv_addr = srv_listener.local_addr().unwrap();
    let srv_task = tokio::spawn(async move {
        let _ = axum::serve(srv_listener, app).await;
    });

    let mut ws_req =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(format!(
            "ws://{srv_addr}/ws"
        ))
        .unwrap();
    ws_req.headers_mut().insert(
        header::AUTHORIZATION,
        header::HeaderValue::from_str(&ws_op_bearer).unwrap(),
    );
    let (mut state_ws, _) = tokio_tungstenite::connect_async(ws_req).await.unwrap();

    // Downgrade `ws_operator` in SQLite to `viewer` while the `/ws` connection is already open
    state.db.update_user_role("ws_operator", "viewer").unwrap();
    state.comms.set_camera_flags(
        "front_door",
        Some(true),
        Some(true),
        Some(true),
        Some(true),
        Some(true),
    );
    state_ws
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({
                "topic": "front_door/record/set",
                "payload": "OFF"
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        state.comms.get_camera_flags("front_door").record,
        "downgraded viewer on existing /ws connection must not be able to mutate camera record state"
    );

    srv_task.abort();
    let _ = std::fs::remove_dir_all(&tmp_dir);
}
