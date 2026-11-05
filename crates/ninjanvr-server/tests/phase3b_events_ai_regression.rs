use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use ninjanvr_db::{EventDto, RecordingRow, ReviewItemDto};
use ninjanvr_server::{
    AppState, JwtClaims, build_router, build_router_with_auth, build_zip_archive,
    create_dataset_sample_from_event, new_app_state, parse_zip_archive,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn setup_phase3b_test_env() -> (PathBuf, AppState, Router, String, String, String) {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr_p3b_audit_{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web-v2");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(media_dir.join("clips")).unwrap();
    std::fs::create_dir_all(media_dir.join("dataset")).unwrap();
    std::fs::create_dir_all(media_dir.join("recordings")).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    let yaml = format!(
        r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
lpr:
  enabled: true
  known_plates:
    VIP9Z99: "Executive Car"
genai:
  enabled: true
  provider: openai
  model: gpt-4o-mini
  api_key: "sk-saved-admin-secret"
  api_key_env: "OPENAI_API_KEY"
  base_url: "https://saved-llm.example.invalid/v1"
cameras:
  front_door:
    enabled: true
    detect:
      width: 1280
      height: 720
      fps: 6
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    detect:
      width: 1920
      height: 1080
      fps: 15
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir, false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("scoped_fd", "pass1234", "operator", "front_door", 1000)
        .unwrap();

    let app = build_router_with_auth(state.clone(), true);
    let now = unix_now();
    let mint = |sub: &str, role: &str, cams: &str| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: Some(cams.to_string()),
                iat: now,
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };
    let admin_bearer = format!("Bearer {}", mint("admin", "admin", "*"));
    let scoped_admin_bearer = format!("Bearer {}", mint("scoped_fd", "operator", "front_door"));
    let scoped_op_bearer = format!("Bearer {}", mint("scoped_fd", "operator", "front_door"));
    (
        tmp_dir,
        state,
        app,
        admin_bearer,
        scoped_admin_bearer,
        scoped_op_bearer,
    )
}

#[tokio::test]
async fn p3b_01_02_false_positive_signatures_events_explore_and_bulk_bounds_regression() {
    let (tmp_dir, state, app, admin_bearer, scoped_admin_bearer, scoped_op_bearer) =
        setup_phase3b_test_env().await;

    for (id, cam, ts) in [
        ("ev_fd_1", "front_door", 1000.0),
        ("ev_by_1", "backyard", 2000.0),
        ("ev_by_2", "backyard", 2005.0),
    ] {
        state
            .db
            .upsert_event(&EventDto {
                id: id.to_string(),
                camera: cam.to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: ts,
                end_time: Some(ts + 4.0),
                has_clip: false,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.91),
                false_positive: Some(false),
                box_coords: Some(json!([0.1, 0.1, 0.4, 0.8])),
                data: json!({ "box": [0.1, 0.1, 0.4, 0.8] }),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.91),
                stationary: false,
            })
            .unwrap();
    }
    state.db.set_event_false_positive("ev_by_1", true).unwrap();
    let sigs = state
        .db
        .list_false_positive_signatures(Some("backyard"))
        .unwrap();
    assert_eq!(sigs.len(), 1);
    let by_sig_id = sigs[0]["id"].as_i64().unwrap();

    // 1. Scoped caller querying GET /api/false_positives/signatures?camera=backyard gets []
    let r_list_forbid = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/false_positives/signatures?camera=backyard")
                .header(header::AUTHORIZATION, &scoped_op_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_list_forbid.status(), StatusCode::OK);
    let list_json: Value =
        serde_json::from_slice(&to_bytes(r_list_forbid.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(list_json, json!([]));

    // 2. Scoped caller restricted to front_door cannot delete backyard signature (both via auth_gate and direct handler)
    let direct_app = build_router(state.clone());
    for router in [&app, &direct_app] {
        let r_del_forbid = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/false_positives/signatures/{by_sig_id}"))
                    .header(header::AUTHORIZATION, &scoped_admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_del_forbid.status(), StatusCode::FORBIDDEN);
    }
    assert_eq!(
        state
            .db
            .list_false_positive_signatures(Some("backyard"))
            .unwrap()
            .len(),
        1,
        "forbidden signature delete must not remove backyard signature"
    );

    // 3. GET /api/events/explore for scoped user returns front_door event with scoped event_count=1 (not global 3)
    let r_explore = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events/explore?limit=1")
                .header(header::AUTHORIZATION, &scoped_op_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_explore.status(), StatusCode::OK);
    let explore_arr: Vec<Value> =
        serde_json::from_slice(&to_bytes(r_explore.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(explore_arr.len(), 1);
    assert_eq!(explore_arr[0]["id"], "ev_fd_1");
    assert_eq!(
        explore_arr[0]["event_count"], 1,
        "scoped /api/events/explore must not leak global cross-camera event_count"
    );

    // 4. Oversized bulk ID arrays (> 1000) are rejected with 400 BAD_REQUEST
    let oversized_ids: Vec<String> = (0..1001).map(|i| format!("id_{i}")).collect();
    for (method, uri, payload) in [
        (
            "POST",
            "/api/reviews/viewed",
            json!({ "ids": oversized_ids, "reviewed": true }),
        ),
        (
            "POST",
            "/api/reviews/delete",
            json!({ "ids": oversized_ids }),
        ),
        (
            "DELETE",
            "/api/events",
            json!({ "event_ids": oversized_ids }),
        ),
    ] {
        let r_bulk = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_bulk.status(),
            StatusCode::BAD_REQUEST,
            "oversized bulk payload on {method} {uri} must return 400 BAD_REQUEST"
        );
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn p3b_03_04_05_genai_ssrf_nonstandard_ips_key_exfiltration_bounded_body_and_privacy_mask() {
    let (tmp_dir, state, app, admin_bearer, _scoped_admin_bearer, _scoped_op_bearer) =
        setup_phase3b_test_env().await;

    // 1. Non-standard IPv4 notations, backslash port confusion, AWS IPv6 IMDS, and DNS rebinding domains are rejected
    for bad_base_url in [
        "http://2852039166/v1",
        "http://0xa9fea9fe/v1",
        "http://0251.0376.0251.0376/v1",
        "http://169.254.1/v1",
        "http://127.1:11434/v1",
        "http://0177.0.0.1:11434/v1",
        "http://127.0.0.1:5000\\api",
        "http://[::169.254.169.254]/v1",
        "http://[fd00:ec2::254]/latest",
        "http://100.100.100.200/latest",
        "https://metadata.google.internal./v1",
        "http://169.254.169.254.nip.io/v1",
        "http://127.0.0.1.sslip.io/v1",
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/genai/validate")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "provider": "openai",
                            "api_key": "sk-test",
                            "base_url": bad_base_url
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::BAD_REQUEST,
            "SSRF bypass base_url '{bad_base_url}' must be rejected with 400 BAD_REQUEST"
        );
    }

    // 2. Spin up a local mock GenAI server that records Authorization headers and image payloads,
    //    and can also return an oversized (> 2 MiB) response body.
    let captured_auth: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured_body: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let oversize_mode: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

    let auth_clone = captured_auth.clone();
    let oversize_clone = oversize_mode.clone();
    let body_clone = captured_body.clone();
    let mock_router = Router::new()
        .route(
            "/v1/models",
            get(move |headers: axum::http::HeaderMap| {
                let auth_hdr = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                *auth_clone.lock().unwrap() = auth_hdr;
                let is_oversize = *oversize_clone.lock().unwrap();
                async move {
                    if is_oversize {
                        let huge = "A".repeat(3 * 1024 * 1024);
                        return (StatusCode::OK, huge).into_response();
                    }
                    Json(json!({ "data": [{ "id": "gpt-4o-mini" }] })).into_response()
                }
            }),
        )
        .route(
            "/v1/chat/completions",
            post(move |body: String| {
                *body_clone.lock().unwrap() = body;
                async move {
                    Json(json!({
                        "choices": [{
                            "message": {
                                "content": "{\"is_true_positive\": true, \"confidence\": 0.95, \"reason\": \"ok\"}"
                            }
                        }]
                    }))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mock_base = format!("http://{addr}/v1");
    let srv_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, mock_router).await;
    });

    // 2a. Changing base_url while passing api_key: "$NINJANVR_TEST_GENAI_SECRET" must NOT exfiltrate the env var
    //     and must NOT clobber state.config.read().await.genai.api_key!
    unsafe {
        std::env::set_var("NINJANVR_TEST_GENAI_SECRET", "env-super-secret-value-123");
    }
    let r_val = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/genai/validate")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "provider": "openai",
                        "base_url": mock_base,
                        "api_key": "$NINJANVR_TEST_GENAI_SECRET",
                        "api_key_env": "NINJANVR_TEST_GENAI_SECRET"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_val.status(), StatusCode::OK);
    assert!(
        captured_auth.lock().unwrap().is_none(),
        "Environment variable API key must never be sent to an untrusted custom base_url"
    );
    assert_eq!(
        state.config.read().await.genai.api_key.as_deref(),
        Some("sk-saved-admin-secret"),
        "Validating against a custom base_url must not overwrite the saved API key in memory"
    );

    // 2b. Oversized (> 2 MiB) GenAI response body is rejected with 502 BAD_GATEWAY without OOM
    *oversize_mode.lock().unwrap() = true;
    let r_huge = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/genai/validate")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "provider": "openai",
                        "base_url": mock_base,
                        "api_key": "sk-custom"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_huge.status(), StatusCode::BAD_GATEWAY);
    *oversize_mode.lock().unwrap() = false;

    // 3. Privacy mask redaction when sending event snapshot to external GenAI Judge
    let mut white_img = image::RgbImage::new(64, 64);
    for p in white_img.pixels_mut() {
        *p = image::Rgb([250, 250, 250]);
    }
    let white_webp = ninjanvr_vision::encode_image_webp(&white_img).unwrap();
    std::fs::write(
        state
            .media_dir
            .join("clips/front_door-ev_masked_ai-clean.webp"),
        &white_webp,
    )
    .unwrap();
    state
        .db
        .upsert_event(&EventDto {
            id: "ev_masked_ai".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 3000.0,
            end_time: Some(3005.0),
            has_clip: false,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.9),
            false_positive: Some(false),
            box_coords: Some(json!([0.1, 0.1, 0.5, 0.5])),
            data: json!({ "box": [0.1, 0.1, 0.5, 0.5] }),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.9),
            stationary: false,
        })
        .unwrap();

    {
        let mut cfg = state.config.write().await;
        cfg.genai.provider = "openai".to_string();
        cfg.genai.base_url = Some(mock_base.clone());
        cfg.genai.model = "gpt-4o-mini".to_string();
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask =
            json!(["0.0,0.0,1.0,0.0,1.0,1.0,0.0,1.0"]);
    }

    let r_verify = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events/ev_masked_ai/verify-ai")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_verify.status(), StatusCode::OK);
    let sent_json: Value = serde_json::from_str(&captured_body.lock().unwrap()).unwrap();
    let data_url = sent_json["messages"][0]["content"][1]["image_url"]["url"]
        .as_str()
        .unwrap();
    let (_, b64_part) = data_url.split_once(";base64,").unwrap();
    let sent_img_bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_part)
        .unwrap();
    let sent_rgb = image::load_from_memory(&sent_img_bytes).unwrap().to_rgb8();
    assert!(
        sent_rgb.get_pixel(32, 32).0[0] < 30,
        "Snapshot sent to external GenAI provider must have privacy_mask redacted"
    );

    srv_handle.abort();
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn p3b_06_07_zip_archive_hardening_and_dataset_scoping_containment_and_privacy_mask() {
    // 1. parse_zip_archive rejects truncated CD name_len (no panic!), Zip-Slip paths, and overlapping-offset Zip-Bombs
    let valid_zip = build_zip_archive(&[("data.yaml".to_string(), b"nc: 15\n".to_vec())]);
    let cd_pos = valid_zip
        .windows(4)
        .position(|w| w == 0x0201_4b50u32.to_le_bytes())
        .unwrap();
    // Corrupt Central Directory name_len to 9999 (exceeding buffer length) -> must return Err, never panic!
    let mut oob_name_zip = valid_zip.clone();
    oob_name_zip[cd_pos + 28] = 0x0F;
    oob_name_zip[cd_pos + 29] = 0x27;
    assert!(parse_zip_archive(&oob_name_zip).is_err());

    // Craft a Zip-Slip entry name ("../evil.txt" = 11 bytes, same length as "images/a.py")
    let mut slip_zip = build_zip_archive(&[("images/a.py".to_string(), b"print(1)".to_vec())]);
    for pos in [30usize, cd_pos + 46] {
        slip_zip[pos..pos + 11].copy_from_slice(b"../evil.txt");
    }
    assert!(
        parse_zip_archive(&slip_zip).is_err(),
        "parse_zip_archive must reject Zip-Slip '..' entry names"
    );

    // Craft an overlapping-offset Stored Zip-Bomb: 2 Central Directory entries pointing to the same local_offset=0
    // so cumulative extracted size > archive byte length
    let payload = vec![b'X'; 256];
    let crc = ninjanvr_server::crc32_ieee(&payload);
    let mut bomb = Vec::new();
    // Single local file header + 256-byte payload
    bomb.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    bomb.extend_from_slice(&20u16.to_le_bytes());
    bomb.extend_from_slice(&0u16.to_le_bytes());
    bomb.extend_from_slice(&0u16.to_le_bytes());
    bomb.extend_from_slice(&0u16.to_le_bytes());
    bomb.extend_from_slice(&0x5421u16.to_le_bytes());
    bomb.extend_from_slice(&crc.to_le_bytes());
    bomb.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bomb.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bomb.extend_from_slice(&5u16.to_le_bytes());
    bomb.extend_from_slice(&0u16.to_le_bytes());
    bomb.extend_from_slice(b"a.txt");
    bomb.extend_from_slice(&payload);
    let cd_start = bomb.len() as u32;
    for name in [b"a.txt", b"b.txt"] {
        bomb.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        bomb.extend_from_slice(&20u16.to_le_bytes());
        bomb.extend_from_slice(&20u16.to_le_bytes());
        bomb.extend_from_slice(&0u16.to_le_bytes());
        bomb.extend_from_slice(&0u16.to_le_bytes());
        bomb.extend_from_slice(&0u16.to_le_bytes());
        bomb.extend_from_slice(&0x5421u16.to_le_bytes());
        bomb.extend_from_slice(&crc.to_le_bytes());
        bomb.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bomb.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bomb.extend_from_slice(&5u16.to_le_bytes());
        bomb.extend_from_slice(&0u16.to_le_bytes());
        bomb.extend_from_slice(&0u16.to_le_bytes());
        bomb.extend_from_slice(&0u16.to_le_bytes());
        bomb.extend_from_slice(&0u16.to_le_bytes());
        bomb.extend_from_slice(&0u32.to_le_bytes());
        bomb.extend_from_slice(&0u32.to_le_bytes());
        bomb.extend_from_slice(name);
    }
    let cd_size = (bomb.len() as u32) - cd_start;
    bomb.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    bomb.extend_from_slice(&0u16.to_le_bytes());
    bomb.extend_from_slice(&0u16.to_le_bytes());
    bomb.extend_from_slice(&2u16.to_le_bytes());
    bomb.extend_from_slice(&2u16.to_le_bytes());
    bomb.extend_from_slice(&cd_size.to_le_bytes());
    bomb.extend_from_slice(&cd_start.to_le_bytes());
    bomb.extend_from_slice(&0u16.to_le_bytes());
    assert!(
        parse_zip_archive(&bomb).is_err(),
        "overlapping-offset Stored Zip-Bomb must be rejected"
    );

    // 2. Dataset Studio camera scoping, lexical traversal delete prevention, and privacy mask redaction
    let (tmp_dir, state, app, admin_bearer, scoped_admin_bearer, _scoped_op_bearer) =
        setup_phase3b_test_env().await;

    let mut white_img = image::RgbImage::new(64, 64);
    for p in white_img.pixels_mut() {
        *p = image::Rgb([250, 250, 250]);
    }
    let white_webp = ninjanvr_vision::encode_image_webp(&white_img).unwrap();
    std::fs::write(
        state.media_dir.join("clips/front_door-ev_ds_fd-clean.webp"),
        &white_webp,
    )
    .unwrap();
    std::fs::write(
        state.media_dir.join("clips/backyard-ev_ds_by-clean.webp"),
        &white_webp,
    )
    .unwrap();

    for (id, cam) in [("ev_ds_fd", "front_door"), ("ev_ds_by", "backyard")] {
        state
            .db
            .upsert_event(&EventDto {
                id: id.to_string(),
                camera: cam.to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 4000.0,
                end_time: Some(4005.0),
                has_clip: false,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.9),
                false_positive: Some(false),
                box_coords: Some(json!([0.1, 0.1, 0.5, 0.5])),
                data: json!({ "box": [0.1, 0.1, 0.5, 0.5] }),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.9),
                stationary: false,
            })
            .unwrap();
        create_dataset_sample_from_event(&state.db, &state.media_dir, id, "true_positive", None)
            .await
            .unwrap();
    }

    // Scoped admin restricted to front_door cannot update, delete, or auto-annotate ds_ev_ds_by
    for (method, uri, body) in [
        (
            "PUT",
            "/api/dataset/samples/ds_ev_ds_by",
            r#"{"verified":true}"#,
        ),
        ("DELETE", "/api/dataset/samples/ds_ev_ds_by", ""),
        ("POST", "/api/dataset/samples/ds_ev_ds_by/auto-annotate", ""),
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::AUTHORIZATION, &scoped_admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::FORBIDDEN,
            "Camera-restricted caller must receive 403 FORBIDDEN on {method} {uri}"
        );
    }

    // Lexical traversal in sample.image_path must not unlink files outside state.media_dir on DELETE
    let outside_keep = tmp_dir.join("outside_dataset_keep.webp");
    std::fs::write(&outside_keep, b"DO_NOT_DELETE").unwrap();
    state
        .db
        .upsert_dataset_sample(&ninjanvr_db::DatasetSampleDto {
            id: "ds_trav_del".to_string(),
            event_id: None,
            camera: "front_door".to_string(),
            split: "train".to_string(),
            sample_kind: "true_positive".to_string(),
            image_path: state
                .media_dir
                .join("dataset/../../outside_dataset_keep.webp")
                .to_string_lossy()
                .to_string(),
            width: 64,
            height: 64,
            annotations: vec![],
            verified: false,
            night_mode: false,
            created_at: 4000.0,
        })
        .unwrap();
    let r_del_trav = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/dataset/samples/ds_trav_del")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_del_trav.status(), StatusCode::OK);
    assert!(
        outside_keep.is_file(),
        "DELETE /api/dataset/samples/{{id}} must never unlink files outside media_dir"
    );

    // Privacy mask on front_door redacts /api/dataset/samples/ds_ev_ds_fd/image.webp and /api/dataset/export.zip
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask =
            json!(["0.0,0.0,1.0,0.0,1.0,1.0,0.0,1.0"]);
    }
    let r_ds_img = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/dataset/samples/ds_ev_ds_fd/image.webp")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ds_img.status(), StatusCode::OK);
    let ds_img_bytes = to_bytes(r_ds_img.into_body(), 65536).await.unwrap();
    let ds_rgb = image::load_from_memory(&ds_img_bytes).unwrap().to_rgb8();
    assert!(
        ds_rgb.get_pixel(32, 32).0[0] < 30,
        "dataset sample image.webp must apply active camera privacy_mask"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn p3b_08_09_10_faces_sub_labels_plates_recordings_delete_ffprobe_and_snapshots_regression() {
    let (tmp_dir, state, app, admin_bearer, scoped_admin_bearer, scoped_op_bearer) =
        setup_phase3b_test_env().await;

    // Seed front_door and backyard events with distinct sub_labels and license plates
    for (id, cam, sub, plate) in [
        ("ev_fd_sub", "front_door", "Alice", "FD1234"),
        ("ev_by_sub", "backyard", "SecretBob", "BY9999"),
    ] {
        state
            .db
            .upsert_event(&EventDto {
                id: id.to_string(),
                camera: cam.to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 5000.0,
                end_time: Some(5005.0),
                has_clip: false,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: Some(sub.to_string()),
                top_score: Some(0.95),
                false_positive: Some(false),
                box_coords: Some(json!([0.1, 0.1, 0.5, 0.5])),
                data: json!({
                    "sub_label": sub,
                    "recognized_license_plate": plate
                }),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.95),
                stationary: false,
            })
            .unwrap();
    }

    // Enroll face profiles for Alice (linked to ev_fd_sub) and SecretBob (linked to ev_by_sub)
    let faces_dir = state.media_dir.join("faces");
    state
        .detector
        .enroll_face_profile(&faces_dir, "Alice", "ev_fd_sub", b"alice-face")
        .unwrap();
    state
        .detector
        .enroll_face_profile(&faces_dir, "SecretBob", "ev_by_sub", b"bob-face")
        .unwrap();

    // 1. Scoped user calling GET /api/sub_labels sees Alice but NOT SecretBob
    let r_subs = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/sub_labels")
                .header(header::AUTHORIZATION, &scoped_op_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_subs.status(), StatusCode::OK);
    let subs_arr: Vec<String> =
        serde_json::from_slice(&to_bytes(r_subs.into_body(), 8192).await.unwrap()).unwrap();
    assert!(subs_arr.contains(&"Alice".to_string()));
    assert!(
        !subs_arr.contains(&"SecretBob".to_string()),
        "Scoped GET /api/sub_labels must not leak backyard sub_label SecretBob"
    );

    // 2. Scoped user calling GET /api/recognized_license_plates sees FD1234 but NOT BY9999 or global VIP9Z99
    let r_plates = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/recognized_license_plates")
                .header(header::AUTHORIZATION, &scoped_op_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_plates.status(), StatusCode::OK);
    let plates_arr: Vec<String> =
        serde_json::from_slice(&to_bytes(r_plates.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(plates_arr, vec!["FD1234".to_string()]);

    // 3. Scoped user calling GET /api/faces sees Alice but NOT SecretBob
    let r_faces = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/faces")
                .header(header::AUTHORIZATION, &scoped_op_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_faces.status(), StatusCode::OK);
    let faces_json: Value =
        serde_json::from_slice(&to_bytes(r_faces.into_body(), 16384).await.unwrap()).unwrap();
    assert!(faces_json["faces"].get("Alice").is_some());
    assert!(
        faces_json["faces"].get("SecretBob").is_none(),
        "Scoped GET /api/faces must not leak backyard face profile SecretBob"
    );

    // 4. Scoped user calling POST /api/events/nonexistent_ev/sub_label fails closed with 403 FORBIDDEN
    let r_sub_nonexist = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events/nonexistent_ev/sub_label")
                .header(header::AUTHORIZATION, &scoped_op_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"sub_label":"InjectedFace","enroll_face":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_sub_nonexist.status(), StatusCode::FORBIDDEN);
    assert!(
        state
            .detector
            .list_face_profiles(&faces_dir)
            .iter()
            .all(|p| p.name != "InjectedFace"),
        "Scoped operator must not enroll faces on non-existent events"
    );

    // 5. GET /api/ffprobe enforces camera scoping and rejects arbitrary file paths
    let r_ffprobe_forbid = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/ffprobe?paths=camera:backyard")
                .header(header::AUTHORIZATION, &scoped_op_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ffprobe_forbid.status(), StatusCode::FORBIDDEN);

    let r_ffprobe_bad_path = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/ffprobe?paths=/etc/passwd")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ffprobe_bad_path.status(), StatusCode::BAD_REQUEST);

    // 6. POST /api/streams/probe rejects RTSP SSRF to metadata IP and internal NVR HTTP ports
    for bad_rtsp in [
        "rtsp://169.254.169.254:554/live",
        "rtsp://[fe80::1]:554/live",
        "rtsp://0.0.0.0:554/live",
        "rtsp://127.0.0.1:5000/api",
        "rtsp://127.0.0.1:1984/api",
    ] {
        let r_probe = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/streams/probe")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(json!({ "url": bad_rtsp }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_probe.status(),
            StatusCode::BAD_REQUEST,
            "Disallowed RTSP probe URL {bad_rtsp} must return 400 BAD_REQUEST"
        );
    }

    // 7. DELETE /api/{camera}/recordings enforces path containment (never unlinks files outside media_dir)
    //    and enforces handler-level camera scoping
    let outside_rec_keep = tmp_dir.join("outside_recording_keep.mp4");
    std::fs::write(&outside_rec_keep, b"DO_NOT_DELETE_RECORDING").unwrap();
    state
        .db
        .insert_recording(&RecordingRow {
            id: "rec_trav_1".to_string(),
            camera: "front_door".to_string(),
            path: outside_rec_keep.to_string_lossy().to_string(),
            start_time: 6000.0,
            end_time: 6010.0,
            duration: 10.0,
            motion: 1,
            objects: 1,
            dbfs: -20,
            segment_size: 1.0,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

    // Scoped caller restricted to front_door cannot read or delete backyard recordings even via direct handler
    let direct_app = build_router(state.clone());
    for (method, uri) in [
        ("GET", "/api/backyard/recordings"),
        ("GET", "/api/backyard/recordings/summary"),
        ("DELETE", "/api/backyard/recordings?after=0&before=99999"),
    ] {
        let r_del_by_recs = direct_app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::AUTHORIZATION, &scoped_admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_del_by_recs.status(), StatusCode::FORBIDDEN);
    }

    let r_del_fd_recs = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/front_door/recordings?after=5990&before=6020")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_del_fd_recs.status(), StatusCode::OK);
    assert!(
        outside_rec_keep.is_file(),
        "DELETE /api/{{camera}}/recordings must never unlink files outside state.media_dir"
    );

    // 8. Event snapshot/thumbnail endpoints apply privacy_mask and prevent cross-camera suffix collision
    let mut white_img = image::RgbImage::new(64, 64);
    for p in white_img.pixels_mut() {
        *p = image::Rgb([250, 250, 250]);
    }
    let white_webp = ninjanvr_vision::encode_image_webp(&white_img).unwrap();
    let white_jpg = ninjanvr_vision::encode_image_jpeg(&white_img).unwrap();
    std::fs::write(
        state
            .media_dir
            .join("clips/backyard-secret-ev_suffix-clean.webp"),
        &white_webp,
    )
    .unwrap();
    std::fs::write(state.media_dir.join("clips/ev_suffix.jpg"), &white_jpg).unwrap();

    state
        .db
        .upsert_event(&EventDto {
            id: "ev_suffix".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 7000.0,
            end_time: Some(7005.0),
            has_clip: false,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.9),
            false_positive: Some(false),
            box_coords: None,
            data: json!({}),
            thumbnail: Some(base64::engine::general_purpose::STANDARD.encode(&white_webp)),
            area: None,
            ratio: None,
            score: Some(0.9),
            stationary: false,
        })
        .unwrap();

    // Activate privacy_mask on front_door: both snapshot.jpg and snapshot-clean.webp must be redacted!
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask =
            json!(["0.0,0.0,1.0,0.0,1.0,1.0,0.0,1.0"]);
    }
    for snap_uri in [
        "/api/events/ev_suffix/snapshot.jpg",
        "/api/events/ev_suffix/snapshot-clean.webp",
        "/api/events/ev_suffix/thumbnail.webp",
    ] {
        let r_snap = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(snap_uri)
                    .header(header::AUTHORIZATION, &scoped_op_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_snap.status(), StatusCode::OK);
        let snap_bytes = to_bytes(r_snap.into_body(), 65536).await.unwrap();
        let snap_rgb = image::load_from_memory(&snap_bytes).unwrap().to_rgb8();
        assert!(
            snap_rgb.get_pixel(32, 32).0[0] < 30,
            "{snap_uri} must redact active camera privacy_mask and never leak unmasked backyard suffix file"
        );
    }

    // 9. Model upload rejects path traversal in filename, weights_filename, and labelmap_filename
    for bad_upload in [
        json!({ "filename": "../evil.onnx" }),
        json!({ "filename": "sub/evil.onnx" }),
        json!({ "filename": "good.onnx", "weights_filename": "../evil.bin" }),
        json!({ "filename": "good.onnx", "labelmap_filename": "../evil.txt" }),
    ] {
        let r_up = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/detectors/models/upload")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(bad_upload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_up.status(),
            StatusCode::BAD_REQUEST,
            "Traversal payload {bad_upload} on model upload must return 400 BAD_REQUEST"
        );
    }

    // Ensure ReviewItemDto import is exercised
    let _ = ReviewItemDto {
        id: "r1".to_string(),
        camera: "front_door".to_string(),
        start_time: 0.0,
        end_time: None,
        severity: "alert".to_string(),
        thumb_path: String::new(),
        data: json!({}),
        has_been_reviewed: false,
    };

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn r2_p3b_cycle2_snapshot_privacy_genai_env_ssrf_symlink_zip_and_timeline_regression() {
    let (tmp_dir, state, app, admin_bearer, scoped_admin_bearer, scoped_op_bearer) =
        setup_phase3b_test_env().await;
    let direct_app = build_router(state.clone());

    // =========================================================================
    // 1. [R2-P3B-01] Deleted/non-existent event snapshot & clip privacy-mask bypass
    // =========================================================================
    let mut white_img = image::RgbImage::new(64, 64);
    for p in white_img.pixels_mut() {
        *p = image::Rgb([250, 250, 250]);
    }
    let white_webp = ninjanvr_vision::encode_image_webp(&white_img).unwrap();
    let white_jpg = ninjanvr_vision::encode_image_jpeg(&white_img).unwrap();

    // Activate privacy mask on backyard
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("backyard").unwrap().privacy_mask =
            json!(["0.0,0.0,1.0,0.0,1.0,1.0,0.0,1.0"]);
    }

    // Write backyard-secret-ev_orphan_suffix-clean.webp (where ev_orphan_suffix is NOT in DB)
    std::fs::write(
        state
            .media_dir
            .join("clips/backyard-secret-ev_orphan_suffix-clean.webp"),
        &white_webp,
    )
    .unwrap();
    let r_orphan_clean = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events/ev_orphan_suffix/snapshot-clean.webp")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_orphan_clean.status(), StatusCode::OK);
    let orphan_bytes = to_bytes(r_orphan_clean.into_body(), 65536).await.unwrap();
    assert_ne!(
        orphan_bytes.as_ref(),
        white_webp.as_slice(),
        "GET /api/events/ev_orphan_suffix/snapshot-clean.webp must never leak unmasked suffix file when event is not in DB"
    );

    // Create an event on backyard with snapshot.jpg, clean.webp, and clip.mp4, then delete the event
    state
        .db
        .upsert_event(&EventDto {
            id: "ev_del_masked".to_string(),
            camera: "backyard".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 8000.0,
            end_time: Some(8005.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.92),
            false_positive: Some(false),
            box_coords: None,
            data: json!({}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.92),
            stationary: false,
        })
        .unwrap();
    let del_jpg = state.media_dir.join("clips/ev_del_masked.jpg");
    let del_webp = state
        .media_dir
        .join("clips/backyard-ev_del_masked-clean.webp");
    let del_mp4 = state.media_dir.join("clips/ev_del_masked.mp4");
    std::fs::write(&del_jpg, &white_jpg).unwrap();
    std::fs::write(&del_webp, &white_webp).unwrap();
    std::fs::write(&del_mp4, b"FAKE_MP4_CLIP_BYTES").unwrap();

    let r_del_ev = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/events/ev_del_masked")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_del_ev.status(), StatusCode::OK);
    assert!(
        !del_jpg.exists() && !del_webp.exists() && !del_mp4.exists(),
        "DELETE /api/events/{{id}} must remove event clip and snapshot files from clips/"
    );

    // Even if an orphan clip file exists on disk without a DB event row, clip.mp4 returns 404
    std::fs::write(&del_mp4, b"ORPHAN_MP4_BYTES").unwrap();
    let r_orphan_clip = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events/ev_del_masked/clip.mp4")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_orphan_clip.status(), StatusCode::NOT_FOUND);

    // =========================================================================
    // 2. [R2-P3B-02] GenAI non-allowlisted env-var exfiltration & provider switch key leak
    // =========================================================================
    let captured_auth: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let auth_clone = captured_auth.clone();
    let mock_router = Router::new()
        .route(
            "/v1/models",
            get(move |headers: axum::http::HeaderMap| {
                let auth_hdr = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                *auth_clone.lock().unwrap() = auth_hdr;
                async move { Json(json!({ "data": [{ "id": "gpt-4o-mini" }] })).into_response() }
            }),
        )
        .route(
            "/api/tags",
            get({
                let auth_clone2 = captured_auth.clone();
                move |headers: axum::http::HeaderMap| {
                    let auth_hdr = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(String::from);
                    *auth_clone2.lock().unwrap() = auth_hdr;
                    async move {
                        Json(json!({ "models": [{ "name": "qwen2.5vl:7b" }] })).into_response()
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mock_base = format!("http://{addr}/v1");
    let srv_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, mock_router).await;
    });

    unsafe {
        std::env::set_var("NINJANVR_FORBIDDEN_INTERNAL_SECRET", "top-secret-jwt-key");
    }
    {
        let mut cfg = state.config.write().await;
        cfg.genai.provider = "openai".to_string();
        cfg.genai.base_url = Some(mock_base.clone());
        cfg.genai.api_key = Some("$NINJANVR_FORBIDDEN_INTERNAL_SECRET".to_string());
        cfg.genai.api_key_env = "NINJANVR_FORBIDDEN_INTERNAL_SECRET".to_string();
    }
    let _ = ninjanvr_server::discover_genai_models(&state.config.read().await.genai).await;
    assert!(
        captured_auth.lock().unwrap().is_none(),
        "discover_genai_models must never resolve non-allowlisted environment variables"
    );

    // Switching provider in /api/genai/validate without supplying a new key must not leak saved key
    {
        let mut cfg = state.config.write().await;
        cfg.genai.provider = "openai".to_string();
        cfg.genai.base_url = Some(format!("http://{addr}"));
        cfg.genai.api_key = Some("sk-saved-openai-secret".to_string());
    }
    *captured_auth.lock().unwrap() = None;
    let r_switch_prov = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/genai/validate")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "provider": "ollama" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_switch_prov.status(), StatusCode::OK);
    assert!(
        captured_auth.lock().unwrap().is_none(),
        "Switching GenAI provider must not leak saved API key from previous provider"
    );

    // Camera-restricted caller on direct_app cannot call POST /api/genai/validate or /api/detectors/models/upload
    for (uri, payload) in [
        ("/api/genai/validate", json!({ "provider": "ollama" })),
        (
            "/api/detectors/models/upload",
            json!({ "filename": "scoped_attempt.onnx" }),
        ),
    ] {
        let r_scoped_forbid = direct_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::AUTHORIZATION, &scoped_admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_scoped_forbid.status(),
            StatusCode::FORBIDDEN,
            "Camera-restricted caller must receive 403 FORBIDDEN on {uri}"
        );
    }

    // =========================================================================
    // 3. [R2-P3B-03] IPv6 Zone-ID SSRF bypass & RTSP password oracle prevention
    // =========================================================================
    for bad_zone_genai in [
        "http://[fe80::1%25eth0]:8080/v1",
        "http://[fe80::1%eth0]:8080/v1",
        "http://[64:ff9b::a9fe:a9fe]/v1",
    ] {
        let r_zone = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/genai/validate")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "provider": "openai",
                            "api_key": "sk-test",
                            "base_url": bad_zone_genai
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_zone.status(), StatusCode::BAD_REQUEST);
    }

    for bad_zone_rtsp in [
        "rtsp://[fe80::1%25eth0]:554/live",
        "rtsp://[fe80::1%eth0]:554/live",
        "rtsp://127.0.0.1.localtest.me:554/live",
        "rtsp://127.0.0.1.lvh.me:554/live",
    ] {
        let r_rtsp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/streams/probe")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(json!({ "url": bad_zone_rtsp }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_rtsp.status(), StatusCode::BAD_REQUEST);
    }

    // Configure front_door with RTSP credentials and verify scoped caller cannot use url equality as a password oracle
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().ffmpeg.inputs[0].path =
            "rtsp://camadmin:SuperSecretCamPass@127.0.0.1:8554/front_door".to_string();
    }
    let r_oracle = direct_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/streams/probe")
                .header(header::AUTHORIZATION, &scoped_admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "camera": "front_door",
                        "url": "rtsp://camadmin:SuperSecretCamPass@127.0.0.1:8554/front_door"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_oracle.status(),
        StatusCode::FORBIDDEN,
        "Scoped caller must not be able to test unredacted RTSP password matches via /api/streams/probe"
    );

    // =========================================================================
    // 4. [R2-P3B-04] Symlink overwrite protection on model upload & dataset subdir confinement
    // =========================================================================
    let external_target = tmp_dir.join("external_do_not_overwrite.onnx");
    std::fs::write(&external_target, b"ORIGINAL_EXTERNAL_FILE").unwrap();
    let models_dir = state.media_dir.join("models");
    std::fs::create_dir_all(&models_dir).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&external_target, models_dir.join("symlinked.onnx")).unwrap();
        let r_sym_up = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/detectors/models/upload")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "filename": "symlinked.onnx",
                            "model_base64": base64::engine::general_purpose::STANDARD.encode(b"PWNED_BYTES")
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_sym_up.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            std::fs::read(&external_target).unwrap(),
            b"ORIGINAL_EXTERNAL_FILE",
            "Model upload must refuse to overwrite symlink targets before writing"
        );
    }

    // Dataset sample pointing to media_dir/recordings/ must not unlink the recording on DELETE
    let rec_keep = state.media_dir.join("recordings/keep_segment.mp4");
    std::fs::write(&rec_keep, b"IMPORTANT_RECORDING_SEGMENT").unwrap();
    state
        .db
        .upsert_dataset_sample(&ninjanvr_db::DatasetSampleDto {
            id: "ds_cross_subdir".to_string(),
            event_id: None,
            camera: "front_door".to_string(),
            split: "train".to_string(),
            sample_kind: "true_positive".to_string(),
            image_path: rec_keep.to_string_lossy().to_string(),
            width: 64,
            height: 64,
            annotations: vec![],
            verified: false,
            night_mode: false,
            created_at: 8500.0,
        })
        .unwrap();
    let r_del_cross = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/dataset/samples/ds_cross_subdir")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_del_cross.status(), StatusCode::OK);
    assert!(
        rec_keep.is_file(),
        "DELETE /api/dataset/samples/{{id}} must only unlink files inside media_dir/dataset/"
    );

    // =========================================================================
    // 5. [R2-P3B-05] Scoped GET /api/timeline?camera=all & GET /api/events?cameras=
    // =========================================================================
    state
        .db
        .insert_timeline_entry(
            9000.0,
            "front_door",
            "tracked_object",
            Some("ev_fd_sub"),
            "entered_zone",
            &json!({"label": "person"}),
        )
        .unwrap();
    state
        .db
        .insert_timeline_entry(
            9001.0,
            "backyard",
            "tracked_object",
            Some("ev_by_sub"),
            "entered_zone",
            &json!({"label": "person"}),
        )
        .unwrap();

    for uri in ["/api/timeline?camera=all", "/api/timeline?camera="] {
        let r_tl = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::AUTHORIZATION, &scoped_op_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_tl.status(), StatusCode::OK);
        let tl_arr: Vec<Value> =
            serde_json::from_slice(&to_bytes(r_tl.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(
            tl_arr.len(),
            1,
            "Scoped caller on {uri} must receive their permitted camera's timeline entries"
        );
        assert_eq!(tl_arr[0]["camera"], "front_door");
    }

    let r_ev_empty_cams = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events?cameras=")
                .header(header::AUTHORIZATION, &scoped_op_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ev_empty_cams.status(), StatusCode::OK);
    let ev_arr: Vec<Value> =
        serde_json::from_slice(&to_bytes(r_ev_empty_cams.into_body(), 16384).await.unwrap())
            .unwrap();
    assert!(
        ev_arr.iter().all(|e| e["camera"] == "front_door"),
        "Scoped caller on /api/events?cameras= must receive only permitted front_door events"
    );

    srv_handle.abort();
    let _ = std::fs::remove_dir_all(&tmp_dir);
}
