use super::*;
use axum::body::to_bytes;
use axum::http::Request;
use axum_extra::extract::cookie::Cookie;
use futures_util::SinkExt;
use hmac::{Hmac, KeyInit, Mac};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use sha2::{Digest, Sha256};
use tokio_tungstenite::tungstenite::protocol::Message as TungMsg;
use tower::ServiceExt;

#[test]
fn review_preview_window_covers_the_review_with_padding_and_caps() {
    assert_eq!(
        review_preview_window_ms(100.0, Some(108.0)),
        Some((99_000, 109_000))
    );
    // Open-ended reviews cover 15 s; long ones are capped at 60 s.
    assert_eq!(
        review_preview_window_ms(100.0, None),
        Some((99_000, 116_000))
    );
    assert_eq!(
        review_preview_window_ms(100.0, Some(5_000.0)),
        Some((99_000, 161_000))
    );
    // An end before the start falls back to 15 s; the left pad never goes negative.
    assert_eq!(review_preview_window_ms(0.5, Some(0.1)), Some((0, 16_500)));
    assert_eq!(review_preview_window_ms(f64::NAN, None), None);
    assert_eq!(review_preview_window_ms(-1.0, None), None);
    assert_eq!(
        review_preview_window_ms(10.0, Some(f64::INFINITY)),
        Some((9_000, 26_000))
    );
}

struct TestApp {
    tmp_dir: PathBuf,
    cfg_path: PathBuf,
    state: AppState,
    app: Router,
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp_dir);
    }
}

async fn setup_test_app() -> TestApp {
    let tmp_dir = std::env::temp_dir().join(format!("ninjanvr-srv-test-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::write(
        web_dir.join("index.html"),
        "<html><body>NinjaNVR Console v2</body></html>",
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
    let app = build_router(state.clone());

    TestApp {
        tmp_dir,
        cfg_path,
        state,
        app,
    }
}

fn make_mp4_box(box_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let total = (8 + payload.len()) as u32;
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(box_type);
    out.extend_from_slice(payload);
    out
}

fn make_mp4_full_box(box_type: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let total = (12 + body.len()) as u32;
    let mut out = Vec::with_capacity(12 + body.len());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(box_type);
    let vf = ((version as u32) << 24) | (flags & 0x00FF_FFFF);
    out.extend_from_slice(&vf.to_be_bytes());
    out.extend_from_slice(body);
    out
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
async fn router_serves_version_config_and_dual_ui_routing() {
    let ctx = setup_test_app().await;
    let app = &ctx.app;

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

    let resp = app
        .clone()
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
    assert!(
        std::str::from_utf8(&body)
            .unwrap()
            .contains("NinjaNVR Console v2")
    );

    let resp_legacy_cookie = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/review")
                .header(header::COOKIE, "ninjanvr_ui=v1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_legacy_cookie.status(), StatusCode::OK);
    let body_legacy = to_bytes(resp_legacy_cookie.into_body(), 4096)
        .await
        .unwrap();
    assert!(
        std::str::from_utf8(&body_legacy)
            .unwrap()
            .contains("NinjaNVR Console v2")
    );

    let resp_switch = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/ui/switch?target=v1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_switch.status(), StatusCode::FOUND);
    assert!(
        resp_switch
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("ninjanvr_ui=v2")
    );

    let mut cfg_patch = val.clone();
    cfg_patch["cameras"]["front_door"]["detect"]["fps"] = json!(8);
    cfg_patch["ui"] = json!({
        "floorplan": {
            "background_data_url": "data:image/svg+xml;base64,PHN2Zy8+",
            "opacity": 0.65,
            "rooms": [
                {
                    "id": "escritorio",
                    "label": "ESCRITÓRIO",
                    "x": 0.08,
                    "y": 0.10,
                    "w": 0.38,
                    "h": 0.36
                }
            ],
            "walls": [
                { "points": [0.08, 0.46, 0.46, 0.46] }
            ]
        }
    });
    let resp_struct = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/structured")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&cfg_patch).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_struct.status(), StatusCode::OK);
    let struct_body: Value =
        serde_json::from_slice(&to_bytes(resp_struct.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(struct_body["success"], true);
    assert_eq!(
        struct_body["config"]["cameras"]["front_door"]["detect"]["fps"],
        8
    );
    assert_eq!(
        struct_body["config"]["ui"]["floorplan"]["rooms"][0]["id"],
        "escritorio"
    );
    assert!(
        (struct_body["config"]["ui"]["floorplan"]["walls"][0]["points"][0]
            .as_f64()
            .unwrap()
            - 0.08)
            .abs()
            < 1e-4
    );
    assert!(
        (struct_body["config"]["ui"]["floorplan"]["walls"][0]["points"][1]
            .as_f64()
            .unwrap()
            - 0.46)
            .abs()
            < 1e-4
    );
    let on_disk_yaml = std::fs::read_to_string(&ctx.cfg_path).unwrap();
    assert!(on_disk_yaml.contains("admin:supersecret@192.0.2.10:554/stream1"));
    assert!(on_disk_yaml.contains("escritorio"));

    let resp_probe_err = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/streams/probe")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"url":"rtsp://admin:topsecretpass@192.0.2.99:554/unreachable_stream"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_probe_err.status(), StatusCode::OK);
    let probe_err_bytes = to_bytes(resp_probe_err.into_body(), 8192).await.unwrap();
    let probe_err_str = std::str::from_utf8(&probe_err_bytes).unwrap();
    assert!(
        !probe_err_str.contains("topsecretpass"),
        "probe output must scrub RTSP credentials"
    );
    let probe_err_json: Value = serde_json::from_slice(&probe_err_bytes).unwrap();
    assert_eq!(probe_err_json["ok"], false);

    let resp_prof_set = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/profile")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"profile":"home"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_prof_set.status(), StatusCode::OK);
    let resp_prof_get = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/profile/active")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let prof_json: Value =
        serde_json::from_slice(&to_bytes(resp_prof_get.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(prof_json["active_profile"], "home");

    for aux_uri in [
        "/version",
        "/api/ui/version",
        "/api/profile",
        "/api/config/raw",
        "/api/config/raw_paths",
        "/api/config/schema.json",
        "/api/detectors/catalog",
        "/api/ffmpeg/presets",
        "/api/ffprobe",
        "/api/keyframe_analysis",
        "/api/profiles",
        "/api/labels",
        "/api/sub_labels",
        "/api/audio_labels",
        "/api/recognized_license_plates",
        "/api/classification/attributes",
        "/api/classification/custom",
        "/api/genai/models",
        "/api/plus/models",
        "/api/faces",
        "/login",
    ] {
        let r = app
            .clone()
            .oneshot(Request::builder().uri(aux_uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "failed on {aux_uri}");
    }

    let resp_cfg_set = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/set")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_cfg_set.status(), StatusCode::OK);

    let resp_restart = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/restart")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_restart.status(), StatusCode::OK);

    let raw_disk = std::fs::read_to_string(&ctx.cfg_path).unwrap();
    let resp_cfg_save = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/save")
                .body(Body::from(raw_disk))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_cfg_save.status(), StatusCode::OK);

    let resp_cat = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/detectors/catalog")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_cat.status(), StatusCode::OK);
    let cat_json: Value =
        serde_json::from_slice(&to_bytes(resp_cat.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(cat_json["active_backend"], "openvino");
    assert!(cat_json["openvino_available"].is_boolean());
    assert!(cat_json["ort_available"].is_boolean());
    let dev_arr = cat_json["devices"].as_array().unwrap();
    assert!(dev_arr.iter().any(|d| d["id"] == "CUDA"));
    assert!(dev_arr.iter().any(|d| d["id"] == "ROCM"));
}

#[tokio::test]
async fn router_enforces_rbac_camera_scoping_and_api_tokens() {
    let ctx = setup_test_app().await;
    let app = &ctx.app;
    let state = &ctx.state;

    let resp_create_user = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"username":"operator2","password":"strong-password","role":"viewer"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_create_user.status(), StatusCode::OK);

    let resp_role = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/users/operator2/role")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"role":"admin"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_role.status(), StatusCode::OK);

    let resp_cams = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/users/operator2/cameras")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"allowed_cameras":"front_door"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_cams.status(), StatusCode::OK);

    let resp_pw = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/users/operator2/password")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"password":"updated-password"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_pw.status(), StatusCode::OK);

    let resp_del_user = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/users/operator2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_del_user.status(), StatusCode::OK);

    let resp_users_list = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/users")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_users_list.status(), StatusCode::OK);

    let resp_tok_http_create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/tokens")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"temp_tok","scope":"read_only"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_tok_http_create.status(), StatusCode::OK);
    let tok_http_json: Value = serde_json::from_slice(
        &to_bytes(resp_tok_http_create.into_body(), 4096)
            .await
            .unwrap(),
    )
    .unwrap();
    let created_tok_id = tok_http_json["metadata"]["id"].as_str().unwrap();

    let resp_tok_http_del = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/tokens/{created_tok_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_tok_http_del.status(), StatusCode::OK);

    let resp_logout = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/logout")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_logout.status(), StatusCode::SEE_OTHER);

    let resp_proxy_sub = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/camera-proxy/non_existent_cam/cgi-bin/config.cgi")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_proxy_sub.status(), StatusCode::NOT_FOUND);

    let auth_app = build_router_with_auth(state.clone(), true);
    let resp = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    state
        .db
        .create_user_with_cameras("guard1", "guard-pass", "viewer", "front_door", 1000)
        .unwrap();
    let guard_claims = JwtClaims {
        sub: "guard1".to_string(),
        role: "viewer".to_string(),
        allowed_cameras: Some("front_door".to_string()),
        iat: unix_now(),
        exp: 9999999999,
    };
    let guard_token = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &guard_claims,
        &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .unwrap();

    let resp_viewer_save = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/save")
                .header(header::AUTHORIZATION, format!("Bearer {guard_token}"))
                .body(Body::from("version: 0.18-0"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_viewer_save.status(), StatusCode::FORBIDDEN);

    let resp_viewer_ok_cam = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/recordings")
                .header(header::AUTHORIZATION, format!("Bearer {guard_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_viewer_ok_cam.status(), StatusCode::OK);

    let resp_viewer_unauth_cam = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/indoor_bedroom/recordings")
                .header(header::AUTHORIZATION, format!("Bearer {guard_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_viewer_unauth_cam.status(), StatusCode::FORBIDDEN);

    let resp_proxy_viewer = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/camera-proxy/front_door/")
                .header(header::AUTHORIZATION, format!("Bearer {guard_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_proxy_viewer.status(), StatusCode::FORBIDDEN);

    let resp_proxy_unconf = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/camera-proxy/non_existent_cam/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_proxy_unconf.status(), StatusCode::NOT_FOUND);

    let (tok_meta, raw_ro_tok) = state
        .db
        .create_api_token("ha_sensor", "read_only", None)
        .unwrap();
    assert!(raw_ro_tok.starts_with("nnvr_"));
    let tok_id = tok_meta["id"].as_str().unwrap();

    let resp_ro_stats = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .header(header::AUTHORIZATION, format!("Bearer {raw_ro_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ro_stats.status(), StatusCode::OK);

    let resp_ro_events = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events")
                .header(header::AUTHORIZATION, format!("Bearer {raw_ro_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ro_events.status(), StatusCode::OK);

    for admin_path in ["/api/users", "/api/tokens", "/api/config/raw"] {
        let resp_ro_admin = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(admin_path)
                    .header(header::AUTHORIZATION, format!("Bearer {raw_ro_tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ro_admin.status(), StatusCode::FORBIDDEN);
    }

    let resp_ro_mutate = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/profile")
                .header(header::AUTHORIZATION, format!("Bearer {raw_ro_tok}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"profile":"disarmed"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ro_mutate.status(), StatusCode::FORBIDDEN);

    let (_, raw_metrics_tok) = state
        .db
        .create_api_token("prom_scraper", "metrics", None)
        .unwrap();
    let resp_metrics_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/metrics")
                .header(header::AUTHORIZATION, format!("Bearer {raw_metrics_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_metrics_ok.status(), StatusCode::OK);

    let resp_metrics_events = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events")
                .header(header::AUTHORIZATION, format!("Bearer {raw_metrics_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_metrics_events.status(), StatusCode::FORBIDDEN);

    let (_, raw_full_tok) = state
        .db
        .create_api_token("cli_admin", "full", None)
        .unwrap();
    let resp_full_list = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/tokens")
                .header(header::AUTHORIZATION, format!("Bearer {raw_full_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_full_list.status(), StatusCode::OK);

    assert!(state.db.delete_api_token(tok_id).unwrap());
    state.auth_gate.invalidate_tokens();
    let resp_revoked = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .header(header::AUTHORIZATION, format!("Bearer {raw_ro_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_revoked.status(), StatusCode::UNAUTHORIZED);

    let resp_first = auth_app
        .oneshot(
            Request::builder()
                .uri("/api/auth/first_time_login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_first.status(), StatusCode::OK);
}

#[tokio::test]
async fn router_validates_oidc_roles_and_trusted_proxy_cidrs() {
    let ctx = setup_test_app().await;
    let auth_app = build_router_with_auth(ctx.state.clone(), true);

    let oidc_cfg = ninjanvr_config::OidcAuthConfig::default();
    assert_eq!(
        resolve_oidc_role(&json!({ "groups": ["users", "admins"] }), &oidc_cfg),
        "admin"
    );
    assert_eq!(
        resolve_oidc_role(
            &json!({
                "urn:zitadel:iam:org:project:roles": {
                    "ninjanvr-operator": { "12345": "org" }
                }
            }),
            &oidc_cfg
        ),
        "operator"
    );
    assert_eq!(
        resolve_oidc_role(
            &json!({
                "urn:zitadel:iam:org:project:roles": {
                    "ninjanvr-viewer": { "12345": "org" }
                }
            }),
            &oidc_cfg
        ),
        "viewer"
    );
    assert!(ip_matches_trusted_proxies(
        "10.42.5.19",
        &["127.0.0.1/32".to_string(), "10.42.0.0/16".to_string()]
    ));
    assert!(!ip_matches_trusted_proxies(
        "203.0.113.99",
        &["127.0.0.1/32".to_string(), "10.42.0.0/16".to_string()]
    ));

    let resp_spoofed = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .header("Remote-User", "admin")
                .header("x-real-ip", "203.0.113.99")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_spoofed.status(), StatusCode::UNAUTHORIZED);

    let resp_providers = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/providers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_providers.status(), StatusCode::OK);
    let prov_json: Value =
        serde_json::from_slice(&to_bytes(resp_providers.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(prov_json["local_login_enabled"], true);
    assert!(prov_json["oidc"].is_object());

    let tls_cfg = generate_self_signed_tls_config_with_sans(&[
        "192.0.2.10".to_string(),
        "nvr.example.com".to_string(),
    ])
    .unwrap();
    assert_eq!(
        tls_cfg.alpn_protocols,
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    );
    let default_tls = generate_self_signed_tls_config().unwrap();
    assert_eq!(default_tls.alpn_protocols, tls_cfg.alpn_protocols);

    let resp_oidc_login = ctx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/oidc/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_oidc_login.status(), StatusCode::BAD_REQUEST);

    let resp_oidc_cb = ctx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/oidc/callback")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_oidc_cb.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn router_serves_hmac_share_links_and_http_range_video() {
    let ctx = setup_test_app().await;
    let app = &ctx.app;
    let state = &ctx.state;
    let auth_app = build_router_with_auth(state.clone(), true);

    let exports_dir = ctx.tmp_dir.join("exports");
    std::fs::create_dir_all(&exports_dir).unwrap();
    let dummy_mp4 = vec![0x42u8; 4096];
    std::fs::write(exports_dir.join("sample.mp4"), &dummy_mp4).unwrap();

    let resp_mp4 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/exports/sample.mp4")
                .header(header::ACCEPT_ENCODING, "gzip, br")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_mp4.status(), StatusCode::OK);
    assert!(
        resp_mp4.headers().get(header::CONTENT_ENCODING).is_none(),
        "MP4 exports must not be Brotli/Gzip compressed by CompressionLayer"
    );
    assert_eq!(
        resp_mp4
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()),
        Some("4096")
    );

    let resp_exp_err = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/export/front_door/start/1000/end/1060")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"missing_clip"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_exp_err.status(), StatusCode::BAD_REQUEST);
    assert!(state.db.query_exports().unwrap().is_empty());

    let resp_share = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"sample","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_share.status(), StatusCode::OK);
    let share_json: Value =
        serde_json::from_slice(&to_bytes(resp_share.into_body(), 4096).await.unwrap()).unwrap();
    let share_token = share_json["token"].as_str().unwrap();

    let resp_share_page = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{share_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_share_page.status(), StatusCode::OK);

    let resp_share_vid = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{share_token}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_share_vid.status(), StatusCode::OK);

    let resp_share_range = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{share_token}/video.mp4"))
                .header(header::RANGE, "bytes=0-3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_share_range.status(), StatusCode::PARTIAL_CONTENT);
    assert!(
        resp_share_range
            .headers()
            .contains_key(header::CONTENT_RANGE)
    );

    let resp_tampered = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{share_token}tampered"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_tampered.status(), StatusCode::FORBIDDEN);

    let expired_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "sample".to_string(),
            exp: 1000,
            cam: None,
            jti: None,
        },
    );
    let resp_expired = auth_app
        .oneshot(
            Request::builder()
                .uri(format!("/share/{expired_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_expired.status(), StatusCode::GONE);
}

#[tokio::test]
async fn router_handles_storage_degradation_and_prometheus_metrics() {
    let ctx = setup_test_app().await;
    let app = &ctx.app;
    let state = &ctx.state;

    let blocked_media_path = ctx.tmp_dir.join("unwritable_mount_file");
    std::fs::write(&blocked_media_path, b"not_a_directory").unwrap();
    let degraded_state = new_app_state(
        &ctx.cfg_path,
        ctx.tmp_dir.join("web"),
        blocked_media_path.clone(),
        false,
    )
    .await
    .expect("server must boot even when media_dir is unwritable");
    let degraded_app = build_router(degraded_state);
    let resp_deg = degraded_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_deg.status(), StatusCode::OK);
    let deg_json: Value =
        serde_json::from_slice(&to_bytes(resp_deg.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(deg_json["service"]["storage_degraded"], true);
    assert!(deg_json["service"]["storage_error"].is_string());

    std::fs::remove_file(&blocked_media_path).unwrap();
    std::fs::create_dir_all(&blocked_media_path).unwrap();
    let resp_rec = degraded_app
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let rec_json: Value =
        serde_json::from_slice(&to_bytes(resp_rec.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(rec_json["service"]["storage_degraded"], false);
    // The detector cameras actually use is reported, and unmeasured latency is flagged.
    assert_eq!(rec_json["primary_detector"]["measured"], false);
    assert_eq!(rec_json["primary_detector"]["inference_speed"], 0.0);
    assert!(rec_json["primary_detector"]["model_path"].is_string());

    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_prom_seed".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!(["driveway"]),
            start_time: 1700000100.0,
            end_time: Some(1700000112.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.92),
            false_positive: Some(false),
            box_coords: Some(json!([0.1, 0.1, 0.3, 0.5])),
            data: json!({ "score": 0.92, "top_score": 0.92, "type": "object" }),
            thumbnail: None,
            area: Some(12000),
            ratio: Some(0.5),
            score: Some(0.92),
            stationary: false,
        })
        .unwrap();

    let resp_notif_test = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/notifications/test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_notif_test.status(), StatusCode::OK);
    let notif_json: Value =
        serde_json::from_slice(&to_bytes(resp_notif_test.into_body(), 4096).await.unwrap())
            .unwrap();
    assert_eq!(notif_json["success"], true);
    assert_eq!(notif_json["ws_delivered"], true);

    let resp_hist = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats/history")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_hist.status(), StatusCode::OK);

    state.db.run_maintenance().unwrap();
    let resp_prom = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_prom.status(), StatusCode::OK);
    assert!(
        resp_prom
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .starts_with("text/plain; version=0.0.4")
    );
    let prom_body = String::from_utf8(
        to_bytes(resp_prom.into_body(), 32768)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(prom_body.contains("ninjanvr_camera_fps{camera=\"front_door\",role=\"camera\"}"));
    assert!(prom_body.contains("ninjanvr_camera_online{camera=\"front_door\"}"));
    // detector01 never ran a model in this test, so no latency sample may be invented for it.
    assert!(prom_body.contains("# TYPE ninjanvr_detector_inference_ms gauge"));
    assert!(!prom_body.contains("ninjanvr_detector_inference_ms{detector=\"detector01\"}"));
    assert!(prom_body.contains("ninjanvr_storage_used_bytes{mount=\"recordings\"}"));
    assert!(prom_body.contains("ninjanvr_cpu_usage_percent "));
    assert!(prom_body.contains("ninjanvr_memory_usage_bytes "));
    assert!(prom_body.contains("ninjanvr_events_total{camera=\"front_door\",label=\"person\"}"));
}

#[tokio::test]
async fn router_serves_ai_context_mcp_and_debug_reprocess() {
    let ctx = setup_test_app().await;
    let app = &ctx.app;

    let resp_ai_ctx = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/ai/context?hours=24")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ai_ctx.status(), StatusCode::OK);
    let ai_ctx_json: Value =
        serde_json::from_slice(&to_bytes(resp_ai_ctx.into_body(), 16384).await.unwrap()).unwrap();
    assert!(ai_ctx_json["service"]["version"].is_string());
    assert!(ai_ctx_json["cameras"]["front_door"].is_object());
    assert!(ai_ctx_json["digest"]["counts_by_camera"].is_object());
    assert!(ai_ctx_json["digest"]["unreviewed_alerts"].is_array());

    let resp_mcp_list = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_mcp_list.status(), StatusCode::OK);
    let mcp_list_json: Value =
        serde_json::from_slice(&to_bytes(resp_mcp_list.into_body(), 16384).await.unwrap()).unwrap();
    let tools_arr = mcp_list_json["result"]["tools"].as_array().unwrap();
    assert!(tools_arr.iter().any(|t| t["name"] == "get_security_digest"));
    assert!(tools_arr.iter().any(|t| t["name"] == "search_events"));

    let resp_mcp_call = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mcp")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_security_digest","arguments":{"hours":24}}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp_mcp_call.status(), StatusCode::OK);
    let mcp_call_json: Value =
        serde_json::from_slice(&to_bytes(resp_mcp_call.into_body(), 16384).await.unwrap()).unwrap();
    assert!(mcp_call_json["result"]["structuredContent"]["digest"].is_object());

    let resp_reprocess = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/debug/reprocess")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"camera":"front_door","start_time":1700000000,"end_time":1700000020,"override_config":{"min_score":0.95,"min_area":500}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp_reprocess.status(), StatusCode::OK);
    let reproc_json: Value =
        serde_json::from_slice(&to_bytes(resp_reprocess.into_body(), 16384).await.unwrap())
            .unwrap();
    assert_eq!(reproc_json["success"], true);
    assert_eq!(reproc_json["camera"], "front_door");
    let frames_arr = reproc_json["frames"].as_array().unwrap();
    assert!(!frames_arr.is_empty());
    assert!(frames_arr[0]["filtered_reasons"].is_array());
}

#[tokio::test]
async fn router_serves_vod_hls_playlists_init_and_m4s_segments() {
    let ctx = setup_test_app().await;
    let app = &ctx.app;
    let state = &ctx.state;

    let rec_subdir = ctx.tmp_dir.join("recordings/2026-11-01/00/front_door");
    std::fs::create_dir_all(&rec_subdir).unwrap();
    let disk_seg = rec_subdir.join("00.00.mp4");
    let mp4_bytes = build_synthetic_mp4_bytes();
    std::fs::write(&disk_seg, &mp4_bytes).unwrap();

    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "vod_seg_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-01/00/front_door/00.00.mp4".to_string(),
            start_time: 1000.0,
            end_time: 1010.0,
            duration: 10.0,
            motion: 12,
            objects: 1,
            dbfs: -22,
            segment_size: 0.25,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

    let resp_master = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/vod/front_door/start/1000/end/1020/master.m3u8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_master.status(), StatusCode::OK);
    assert_eq!(
        resp_master
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/vnd.apple.mpegurl")
    );
    let master_body = String::from_utf8(
        to_bytes(resp_master.into_body(), 4096)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(master_body.contains("index-v1-a1.m3u8"));

    let resp_idx = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/vod/front_door/start/1000/end/1020/index-v1-a1.m3u8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_idx.status(), StatusCode::OK);
    assert_eq!(
        resp_idx
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/vnd.apple.mpegurl")
    );
    let idx_body =
        String::from_utf8(to_bytes(resp_idx.into_body(), 4096).await.unwrap().to_vec()).unwrap();
    assert!(idx_body.contains("#EXT-X-MAP:URI=\"init-v1-a1.mp4\""));
    assert!(idx_body.contains("seg-1-v1-a1.m4s"));

    let resp_init = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/vod/front_door/start/1000/end/1020/init-v1-a1.mp4")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_init.status(), StatusCode::OK);
    assert_eq!(
        resp_init
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("video/mp4")
    );
    let init_bytes = to_bytes(resp_init.into_body(), 65536).await.unwrap();
    assert!(init_bytes.len() > 64);

    let resp_seg = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/vod/front_door/start/1000/end/1020/seg-1-v1-a1.m4s")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_seg.status(), StatusCode::OK);
    assert_eq!(
        resp_seg
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("video/iso.segment")
    );
    let seg_bytes = to_bytes(resp_seg.into_body(), 65536).await.unwrap();
    assert!(seg_bytes.ends_with(&[0x65u8; 64]));

    let resp_empty = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/vod/front_door/start/9000/end/9010/index.m3u8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_empty.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn router_manages_reviews_events_timeline_and_exports() {
    let ctx = setup_test_app().await;
    let app = &ctx.app;
    let state = &ctx.state;

    let rec_subdir = ctx.tmp_dir.join("recordings/2026-11-01/00/front_door");
    std::fs::create_dir_all(&rec_subdir).unwrap();
    let disk_seg = rec_subdir.join("00.00.mp4");
    std::fs::write(&disk_seg, build_synthetic_mp4_bytes()).unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rev_ev_seg".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-01/00/front_door/00.00.mp4".to_string(),
            start_time: 1700000100.0,
            end_time: 1700000115.0,
            duration: 15.0,
            motion: 40,
            objects: 2,
            dbfs: -18,
            segment_size: 0.5,
            regions: 2,
            motion_heatmap: None,
        })
        .unwrap();

    let clips_dir = ctx.tmp_dir.join("clips");
    std::fs::create_dir_all(&clips_dir).unwrap();
    std::fs::write(clips_dir.join("ev_test_1.jpg"), [0xFF, 0xD8, 0xFF, 0xD9]).unwrap();
    std::fs::write(
        clips_dir.join("front_door-ev_test_1.webp"),
        b"RIFF\x0c\x00\x00\x00WEBPVP8 ",
    )
    .unwrap();

    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_test_1".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!(["driveway"]),
            start_time: 1700000100.0,
            end_time: Some(1700000112.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.92),
            false_positive: Some(false),
            box_coords: Some(json!([0.1, 0.1, 0.3, 0.5])),
            data: json!({ "score": 0.92, "top_score": 0.92, "type": "object" }),
            thumbnail: None,
            area: Some(12000),
            ratio: Some(0.5),
            score: Some(0.92),
            stationary: false,
        })
        .unwrap();

    state
        .db
        .insert_review_segment(&ninjanvr_db::ReviewItemDto {
            id: "rev_test_1".to_string(),
            camera: "front_door".to_string(),
            start_time: 1700000100.0,
            end_time: Some(1700000112.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/front_door-ev_test_1.webp".to_string(),
            data: json!({
                "detections": ["ev_test_1"],
                "objects": ["person"],
                "zones": ["driveway"]
            }),
            has_been_reviewed: false,
        })
        .unwrap();

    state
        .db
        .insert_timeline_entry(
            1700000101.0,
            "front_door",
            "tracked_object",
            Some("ev_test_1"),
            "visible",
            &json!({ "label": "person", "score": 0.92 }),
        )
        .unwrap();

    let resp_rev = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/review?cameras=front_door")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev.status(), StatusCode::OK);
    let rev_list: Value =
        serde_json::from_slice(&to_bytes(resp_rev.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(rev_list.as_array().unwrap().len(), 1);

    let resp_rev_sum = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/review/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_sum.status(), StatusCode::OK);

    let resp_motion = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/review/activity/motion?after=1700000000&before=1700000200")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_motion.status(), StatusCode::OK);
    let motion_arr: Value =
        serde_json::from_slice(&to_bytes(resp_motion.into_body(), 16384).await.unwrap()).unwrap();
    assert!(!motion_arr.as_array().unwrap().is_empty());

    let resp_rev_by_ev = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/review/event/ev_test_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_by_ev.status(), StatusCode::OK);

    let resp_rev_by_id = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/review/rev_test_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_by_id.status(), StatusCode::OK);

    let resp_rev_prev = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/review/rev_test_1/preview")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_prev.status(), StatusCode::OK);

    let resp_rev_viewed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/reviews/viewed")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ids":["rev_test_1"],"reviewed":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_viewed.status(), StatusCode::OK);
    assert!(
        state
            .db
            .get_review_by_id("rev_test_1")
            .unwrap()
            .unwrap()
            .has_been_reviewed
    );

    let resp_rev_unview = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/review/rev_test_1/viewed")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_unview.status(), StatusCode::OK);
    assert!(
        !state
            .db
            .get_review_by_id("rev_test_1")
            .unwrap()
            .unwrap()
            .has_been_reviewed
    );

    let resp_rev_notes = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/review/rev_test_1/notes")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"notes":"Verified alert segment"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_notes.status(), StatusCode::OK);
    assert_eq!(
        state
            .db
            .get_review_by_id("rev_test_1")
            .unwrap()
            .unwrap()
            .data["notes"],
        "Verified alert segment"
    );

    let resp_rev_ids = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/review_ids?ids=rev_test_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_ids.status(), StatusCode::OK);

    let resp_rev_del = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/reviews/delete")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ids":["rev_test_1"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_del.status(), StatusCode::OK);
    assert!(state.db.get_review_by_id("rev_test_1").unwrap().is_none());

    let resp_ev_exp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events/explore?limit=5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ev_exp.status(), StatusCode::OK);

    let resp_ev_sum = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ev_sum.status(), StatusCode::OK);

    let resp_ret_on = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events/ev_test_1/retain")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ret_on.status(), StatusCode::OK);
    assert_eq!(
        state.db.get_event("ev_test_1").unwrap().unwrap()["retain_indefinitely"],
        true
    );

    let resp_ret_off = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/events/ev_test_1/retain")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ret_off.status(), StatusCode::OK);
    assert_eq!(
        state.db.get_event("ev_test_1").unwrap().unwrap()["retain_indefinitely"],
        false
    );

    let resp_fp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/events/ev_test_1/false_positive")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"false_positive":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_fp.status(), StatusCode::OK);

    let resp_sub = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events/ev_test_1/sub_label")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"sub_label":"courier"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_sub.status(), StatusCode::OK);

    let resp_desc = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events/ev_test_1/description")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"description":"Delivery driver at gate"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_desc.status(), StatusCode::OK);

    let resp_ev_notes = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/events/ev_test_1/notes")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"notes":"Courier left parcel behind planter — verified by guard"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ev_notes.status(), StatusCode::OK);
    let ev_updated = state.db.get_event("ev_test_1").unwrap().unwrap();
    assert_eq!(ev_updated["sub_label"], "courier");
    assert_eq!(ev_updated["data"]["description"], "Delivery driver at gate");
    assert_eq!(
        ev_updated["data"]["notes"],
        "Courier left parcel behind planter — verified by guard"
    );

    // Configure known_plates and test PUT /api/events/{id}/license_plate,
    // GET /api/recognized_license_plates, and GET /api/events?plate=ABC1D23
    {
        let mut cfg = state.config.write().await;
        cfg.lpr.enabled = true;
        cfg.lpr
            .known_plates
            .insert("ABC1D23".to_string(), "Family SUV".to_string());
    }
    let resp_lpr_put = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/events/ev_test_1/license_plate")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"recognized_license_plate":"abc-1d23"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_lpr_put.status(), StatusCode::OK);
    let lpr_put_json: Value =
        serde_json::from_slice(&to_bytes(resp_lpr_put.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(lpr_put_json["success"], true);
    assert_eq!(lpr_put_json["recognized_license_plate"], "ABC1D23");
    assert_eq!(lpr_put_json["sub_label"], "Family SUV");

    let resp_plates_list = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/recognized_license_plates")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_plates_list.status(), StatusCode::OK);
    let plates_list_json: Value =
        serde_json::from_slice(&to_bytes(resp_plates_list.into_body(), 4096).await.unwrap())
            .unwrap();
    assert_eq!(plates_list_json, json!(["ABC1D23"]));

    let resp_ev_plate = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events?plate=ABC1D23")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ev_plate.status(), StatusCode::OK);
    let ev_plate_arr: Value =
        serde_json::from_slice(&to_bytes(resp_ev_plate.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(ev_plate_arr.as_array().unwrap().len(), 1);
    assert_eq!(ev_plate_arr[0]["id"], "ev_test_1");
    assert_eq!(
        ev_plate_arr[0]["data"]["recognized_license_plate"],
        "ABC1D23"
    );
    assert_eq!(ev_plate_arr[0]["sub_label"], "Family SUV");

    let resp_thumb = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events/ev_test_1/thumbnail.webp")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_thumb.status(), StatusCode::OK);

    let resp_snap = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events/ev_test_1/snapshot.jpg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_snap.status(), StatusCode::OK);

    let resp_clip = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events/ev_test_1/clip.mp4")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_clip.status(), StatusCode::OK);

    for ev_uri in [
        "/api/events?cameras=front_door",
        "/api/events/search?cameras=front_door",
        "/api/event_ids?ids=ev_test_1",
        "/api/events/ev_test_1",
        "/api/events/ev_test_1/thumbnail.jpg",
        "/api/events/ev_test_1/snapshot-clean.webp",
        "/api/jobs/export",
    ] {
        let r = app
            .clone()
            .oneshot(Request::builder().uri(ev_uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "failed on {ev_uri}");
    }

    let resp_ev_bulk_del = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/events")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"event_ids":["non_existent_ev"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ev_bulk_del.status(), StatusCode::OK);

    let resp_ev_del_one = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/events/ev_test_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ev_del_one.status(), StatusCode::OK);
    assert!(state.db.get_event("ev_test_1").unwrap().is_none());

    let resp_tl = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/timeline?camera=front_door&source_id=ev_test_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_tl.status(), StatusCode::OK);
    let tl_arr: Value =
        serde_json::from_slice(&to_bytes(resp_tl.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(tl_arr.as_array().unwrap().len(), 1);

    let resp_logs = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/logs/ninjanvr")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_logs.status(), StatusCode::OK);
    let logs_json: Value =
        serde_json::from_slice(&to_bytes(resp_logs.into_body(), 16384).await.unwrap()).unwrap();
    assert!(logs_json["lines"].as_array().unwrap().len() >= 2);

    state
        .db
        .insert_export(&ExportRow {
            id: "exp_1".to_string(),
            camera: "front_door".to_string(),
            name: "Initial clip".to_string(),
            date: 1700000200.0,
            video_path: "/media/ninjanvr/exports/exp_1.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_1.webp".to_string(),
            in_progress: false,
        })
        .unwrap();
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_2".to_string(),
            camera: "front_door".to_string(),
            name: "Second clip".to_string(),
            date: 1700000210.0,
            video_path: "/media/ninjanvr/exports/exp_2.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_2.webp".to_string(),
            in_progress: false,
        })
        .unwrap();

    let resp_exps = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/exports")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_exps.status(), StatusCode::OK);
    let exps_arr: Value =
        serde_json::from_slice(&to_bytes(resp_exps.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(exps_arr.as_array().unwrap().len(), 2);

    let resp_ren = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/export/exp_1/rename")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Renamed clip"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ren.status(), StatusCode::OK);

    let resp_del_one = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/export/exp_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_del_one.status(), StatusCode::OK);
    assert_eq!(state.db.query_exports().unwrap().len(), 1);

    let resp_del_bulk = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/exports/delete")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ids":["exp_2"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_del_bulk.status(), StatusCode::OK);
    assert!(state.db.query_exports().unwrap().is_empty());
}

#[tokio::test]
async fn router_manages_cases_recordings_previews_camera_state_and_federation() {
    let ctx = setup_test_app().await;
    let app = &ctx.app;
    let state = &ctx.state;

    let resp_cases_get = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/cases")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_cases_get.status(), StatusCode::OK);

    let resp_cases_post = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/cases")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_cases_post.status(), StatusCode::OK);

    let rec_subdir = ctx.tmp_dir.join("recordings/2026-11-01/00/front_door");
    std::fs::create_dir_all(&rec_subdir).unwrap();
    let disk_seg_1 = rec_subdir.join("00.00.mp4");
    std::fs::write(&disk_seg_1, vec![0x11u8; 1024]).unwrap();

    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "seg_on_disk".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-01/00/front_door/00.00.mp4".to_string(),
            start_time: 2000.0,
            end_time: 2010.0,
            duration: 10.0,
            motion: 1,
            objects: 0,
            dbfs: 0,
            segment_size: 0.001,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "seg_ghost".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-01/00/front_door/00.10.mp4".to_string(),
            start_time: 2010.0,
            end_time: 2020.0,
            duration: 10.0,
            motion: 0,
            objects: 0,
            dbfs: 0,
            segment_size: 0.001,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();

    let resp_rec_sum = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/recordings/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rec_sum.status(), StatusCode::OK);

    let resp_rec_stor = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/recordings/storage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rec_stor.status(), StatusCode::OK);

    let resp_cam_recs = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/recordings?after=1990&before=2030")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_cam_recs.status(), StatusCode::OK);
    let cam_recs_arr: Value =
        serde_json::from_slice(&to_bytes(resp_cam_recs.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(cam_recs_arr.as_array().unwrap().len(), 2);

    let resp_sync = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/recordings/sync")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_sync.status(), StatusCode::OK);
    let sync_json: Value =
        serde_json::from_slice(&to_bytes(resp_sync.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(sync_json["removed_orphans"], 1);

    let resp_del_rec = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/front_door/recordings?after=1995&before=2015")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_del_rec.status(), StatusCode::OK);
    let del_rec_json: Value =
        serde_json::from_slice(&to_bytes(resp_del_rec.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(del_rec_json["deleted_segments"], 1);
    assert_eq!(del_rec_json["freed_bytes"], 1024);
    assert!(!disk_seg_1.exists());

    state
        .db
        .insert_preview(
            "prev_1",
            "front_door",
            "/media/ninjanvr/clips/previews/front_door/prev_1.mp4",
            1000.0,
            1060.0,
            60.0,
        )
        .unwrap();
    let resp_prev = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/preview/front_door/start/900/end/1100")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_prev.status(), StatusCode::OK);
    let prev_arr: Value =
        serde_json::from_slice(&to_bytes(resp_prev.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(prev_arr.as_array().unwrap().len(), 1);

    let resp_cam_state_get = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/state")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_cam_state_get.status(), StatusCode::OK);

    let resp_cam_state = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/state")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"detect":false,"alerts":false}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_cam_state.status(), StatusCode::OK);
    let state_json: Value =
        serde_json::from_slice(&to_bytes(resp_cam_state.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(state_json["detect"], false);
    assert_eq!(state_json["alerts"], false);
    assert_eq!(state_json["record"], true);

    let clips_dir = ctx.tmp_dir.join("clips");
    std::fs::create_dir_all(&clips_dir).unwrap();
    let disk_latest = clips_dir.join("latest_front_door.jpg");
    let fake_jpeg = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0xFF, 0xD9,
    ];
    std::fs::write(&disk_latest, &fake_jpeg).unwrap();
    state.camera_metrics.write().await.insert(
        "front_door".to_string(),
        pipeline::CameraRuntimeMetrics {
            camera_fps: 0.0,
            process_fps: 0.0,
            skipped_fps: 0.0,
            detection_fps: 0.0,
            total_inferences: 0,
            pid: 123,
            last_frame_ts: 1730500123.0,
            night_mode: false,
        },
    );

    for latest_uri in ["/api/front_door/latest.webp", "/api/front_door/latest.jpg"] {
        let resp_latest = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(latest_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_latest.status(), StatusCode::OK);
        assert_eq!(
            resp_latest
                .headers()
                .get("x-last-frame-timestamp")
                .and_then(|v| v.to_str().ok()),
            Some("1730500123")
        );
        let latest_body = to_bytes(resp_latest.into_body(), 4096).await.unwrap();
        assert_eq!(&latest_body[..], &fake_jpeg[..]);
    }

    let resp_fed_nodes = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/federation/nodes")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_fed_nodes.status(), StatusCode::OK);
    let fed_json: Value =
        serde_json::from_slice(&to_bytes(resp_fed_nodes.into_body(), 8192).await.unwrap()).unwrap();
    let nodes_arr = fed_json["nodes"].as_array().unwrap();
    assert!(!nodes_arr.is_empty());
    assert_eq!(nodes_arr[0]["id"], "local");
    assert_eq!(nodes_arr[0]["online"], true);

    let resp_fed_unconf = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/federation/unknown_peer/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_fed_unconf.status(), StatusCode::NOT_FOUND);

    for extra_uri in [
        "/api/recordings/unavailable",
        "/api/front_door/recordings/summary",
        "/api/front_door/ptz/info",
        "/api/preview/front_door/start/900/end/1100/frames",
        "/api/preview/preview_front_door-1000/thumbnail.webp",
        "/api/preview/preview_front_door-1000/thumbnail.jpg",
        "/api/go2rtc/streams",
        "/api/go2rtc/streams/front_door",
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(extra_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "failed on {extra_uri}");
    }

    for (frames_uri, expected) in [
        ("/api/review/missing_review/frames", StatusCode::NOT_FOUND),
        (
            "/api/review/missing_review/frames/0.jpg",
            StatusCode::NOT_FOUND,
        ),
        (
            "/api/review/missing_review/frames/abc.jpg",
            StatusCode::BAD_REQUEST,
        ),
        // Encoded traversal never reaches the handler: the router guard refuses it.
        (
            "/api/review/missing_review/frames/..%2F0.jpg",
            StatusCode::FORBIDDEN,
        ),
        (
            "/api/review/missing_review/frames/0.png",
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(frames_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), expected, "unexpected status on {frames_uri}");
    }

    // The frame list serves preview frames captured for this review's camera and
    // window only (review 5014-5030 -> window 5013-5031).
    state
        .db
        .insert_review_segment(&ninjanvr_db::ReviewItemDto {
            id: "rev_frames".to_string(),
            camera: "front_door".to_string(),
            start_time: 5014.0,
            end_time: Some(5030.0),
            severity: "alert".to_string(),
            thumb_path: String::new(),
            data: json!({ "objects": ["person"] }),
            has_been_reviewed: false,
        })
        .unwrap();
    for ts in [5_000_000u64, 5_014_000, 5_020_500, 5_031_000, 5_040_000] {
        state
            .preview_frames
            .write_frame("front_door", ts, b"\xff\xd8jpeg")
            .unwrap();
    }
    state
        .preview_frames
        .write_frame("other_cam", 5_015_000, b"jpeg")
        .unwrap();
    let resp_frames = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/review/rev_frames/frames")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_frames.status(), StatusCode::OK);
    let frames_json: Value =
        serde_json::from_slice(&to_bytes(resp_frames.into_body(), 8192).await.unwrap()).unwrap();
    let urls: Vec<&str> = frames_json["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["url"].as_str().unwrap())
        .collect();
    assert_eq!(
        urls,
        vec![
            "/api/review/rev_frames/frames/5014000.jpg",
            "/api/review/rev_frames/frames/5020500.jpg",
            "/api/review/rev_frames/frames/5031000.jpg",
        ]
    );
    for (frame_uri, expected) in [
        ("/api/review/rev_frames/frames/5020500.jpg", StatusCode::OK),
        // Exists on disk but outside the review window, or on another camera.
        (
            "/api/review/rev_frames/frames/5040000.jpg",
            StatusCode::NOT_FOUND,
        ),
        (
            "/api/review/rev_frames/frames/5015000.jpg",
            StatusCode::NOT_FOUND,
        ),
        // Inside the window but never captured.
        (
            "/api/review/rev_frames/frames/5016000.jpg",
            StatusCode::NOT_FOUND,
        ),
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(frame_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), expected, "unexpected status on {frame_uri}");
        if expected == StatusCode::OK {
            assert_eq!(
                r.headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some("image/jpeg")
            );
            let body = to_bytes(r.into_body(), 1024).await.unwrap();
            assert_eq!(&body[..], b"\xff\xd8jpeg");
        }
    }

    let resp_webrtc = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/go2rtc/webrtc?src=front_door")
                .body(Body::from("v=0"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp_webrtc.status() == StatusCode::BAD_GATEWAY || resp_webrtc.status().is_success());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let ws_router = app.clone();
    let srv_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, ws_router).await;
    });
    let (mut ws_stream, ws_resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    assert_eq!(ws_resp.status(), StatusCode::SWITCHING_PROTOCOLS);
    let first_msg = ws_stream.next().await.unwrap().unwrap();
    assert!(first_msg.into_text().unwrap().contains("topic"));
    let _ = ws_stream.close(None).await;

    let (_, mse_ws_resp) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/live/mse/api/ws?src=front_door"))
            .await
            .unwrap();
    assert_eq!(mse_ws_resp.status(), StatusCode::SWITCHING_PROTOCOLS);
    srv_handle.abort();
}

#[tokio::test]
async fn test_go2rtc_webrtc_backchannel_proxy_and_upstream_failure() {
    let t = setup_test_app().await;

    let captured_requests: Arc<RwLock<Vec<(String, String, String)>>> =
        Arc::new(RwLock::new(Vec::new()));
    let captured_clone = captured_requests.clone();

    let mock_go2rtc = Router::new().route(
            "/api/webrtc",
            post(
                move |uri: Uri, headers: HeaderMap, body: axum::body::Bytes| {
                    let captured = captured_clone.clone();
                    async move {
                        let query = uri.query().unwrap_or("").to_string();
                        let ct = headers
                            .get(header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let sdp_offer = String::from_utf8_lossy(&body).to_string();
                        captured
                            .write()
                            .await
                            .push((query.clone(), ct, sdp_offer.clone()));

                        let answer_sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=go2rtc-backchannel\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\na=mid:0\r\n".to_string();
                        let status = if query.contains("status=201") {
                            StatusCode::CREATED
                        } else {
                            StatusCode::OK
                        };
                        (
                            status,
                            [(header::CONTENT_TYPE, "application/sdp")],
                            answer_sdp,
                        )
                    }
                },
            ),
        );

    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    let mock_handle = tokio::spawn(async move {
        let _ = axum::serve(mock_listener, mock_go2rtc).await;
    });

    let mut state = t.state.clone();
    state.go2rtc_http_base = format!("http://{mock_addr}");
    let app = build_router(state);

    let sdp_offer = "v=0\r\no=- 42 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\n";

    // 1. Verify 200 OK SDP offer/answer proxy with explicit Content-Type and src=front_door
    let resp_ok = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/go2rtc/webrtc?src=front_door")
                .header(header::CONTENT_TYPE, "application/sdp")
                .body(Body::from(sdp_offer))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ok.status(), StatusCode::OK);
    assert_eq!(
        resp_ok
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/sdp")
    );
    let answer_bytes = to_bytes(resp_ok.into_body(), 64 * 1024).await.unwrap();
    let answer_text = String::from_utf8_lossy(&answer_bytes);
    assert!(answer_text.contains("s=go2rtc-backchannel"));
    assert!(answer_text.contains("a=recvonly"));

    // 2. Verify 201 Created response passthrough and default Content-Type header injection
    let resp_created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/go2rtc/webrtc?src=front_door&status=201")
                .body(Body::from(sdp_offer))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_created.status(), StatusCode::CREATED);
    let created_bytes = to_bytes(resp_created.into_body(), 64 * 1024).await.unwrap();
    assert!(String::from_utf8_lossy(&created_bytes).contains("s=go2rtc-backchannel"));

    // Assert upstream mock received exact query parameters, Content-Type, and SDP offers
    {
        let recorded = captured_requests.read().await;
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].0, "src=front_door");
        assert_eq!(recorded[0].1, "application/sdp");
        assert_eq!(recorded[0].2, sdp_offer);
        assert_eq!(recorded[1].0, "src=front_door&status=201");
        assert_eq!(recorded[1].1, "application/sdp");
        assert_eq!(recorded[1].2, sdp_offer);
    }

    // 2b. When a camera has a dedicated `<camera>_talk` backchannel stream (e.g. Tapo `tapo://`),
    //     audio-only talkback SDP offers (`a=sendonly` or `a=sendrecv`) route to `src=<camera>_talk`,
    //     while video SDP offers stay on `src=<camera>`.
    {
        let mut cfg_w = t.state.config.write().await;
        cfg_w.go2rtc.streams.insert(
            "front_door_talk".to_string(),
            json!(["tapo://cloud_secret@192.0.2.65"]),
        );
    }
    let sdp_sendrecv_audio = "v=0\r\no=- 43 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendrecv\r\n";
    let sdp_with_video = "v=0\r\no=- 44 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=recvonly\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendrecv\r\n";
    for offer_body in [sdp_offer, sdp_sendrecv_audio, sdp_with_video] {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/go2rtc/webrtc?src=front_door&status=201")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from(offer_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
    }
    {
        let recorded = captured_requests.read().await;
        assert_eq!(recorded.len(), 5);
        assert_eq!(recorded[2].0, "src=front_door_talk&status=201");
        assert_eq!(recorded[3].0, "src=front_door_talk&status=201");
        assert_eq!(recorded[4].0, "src=front_door&status=201");
    }

    // 3. Shut down mock go2rtc server and verify 502 Bad Gateway on upstream error
    mock_handle.abort();
    let _ = mock_handle.await;

    let resp_fail = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/go2rtc/webrtc?src=front_door")
                .header(header::CONTENT_TYPE, "application/sdp")
                .body(Body::from(sdp_offer))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_fail.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn api_false_positive_signatures_and_verify_ai_roundtrip() {
    let tmp_dir = std::env::temp_dir().join(format!("ninjanvr-ai-judge-{}", rand::random::<u32>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("config.yml");
    let db_path = tmp_dir.join("ninjanvr.db");
    std::fs::write(
        &cfg_path,
        format!(
            "database:\n  path: {}\nversion: 0.18-0\n",
            db_path.display()
        ),
    )
    .unwrap();

    let state = new_app_state(&cfg_path, tmp_dir.join("web"), tmp_dir.join("media"), false)
        .await
        .unwrap();

    // Spin up a local mock HTTP server simulating Gemini, OpenAI, and Ollama Vision endpoints
    let mock_router = Router::new()
            .route(
                "/v1beta/models/{*model_action}",
                post(|| async {
                    Json(json!({
                        "candidates": [{
                            "content": {
                                "parts": [{
                                    "text": "{\"is_true_positive\": false, \"confidence\": 0.96, \"reason\": \"Static patio fern misclassified as person\"}"
                                }]
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/v1/chat/completions",
                post(|| async {
                    Json(json!({
                        "choices": [{
                            "message": {
                                "content": "```json\n{\"is_true_positive\": true, \"confidence\": 0.94, \"reason\": \"Delivery driver standing at porch\"}\n```"
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/api/generate",
                post(|| async {
                    Json(json!({
                        "response": "{\"is_true_positive\": false, \"confidence\": 0.91, \"reason\": \"Spider web IR reflection\"}"
                    }))
                }),
            )
            .route(
                "/v1beta/models",
                get(|| async {
                    Json(json!({
                        "models": [
                            {
                                "name": "models/gemini-2.5-flash",
                                "supportedGenerationMethods": ["generateContent"]
                            },
                            {
                                "name": "models/gemini-2.5-pro",
                                "supportedGenerationMethods": ["generateContent"]
                            },
                            {
                                "name": "models/text-embedding-004",
                                "supportedGenerationMethods": ["embedContent"]
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/v1/models",
                get(|| async {
                    Json(json!({
                        "data": [
                            { "id": "gpt-4o-mini" },
                            { "id": "gpt-4o" },
                            { "id": "whisper-1" }
                        ]
                    }))
                }),
            )
            .route(
                "/api/tags",
                get(|| async {
                    Json(json!({
                        "models": [
                            { "name": "qwen2.5vl:7b" },
                            { "name": "llava:7b" }
                        ]
                    }))
                }),
            );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    let mock_base = format!("http://{mock_addr}");
    let mock_srv = tokio::spawn(async move {
        let _ = axum::serve(mock_listener, mock_router).await;
    });

    // Insert two test events into SQLite
    for (ev_id, dhash) in [
        ("ev_gemini_fp", 0x1122_3344_5566_7788u64),
        ("ev_openai_tp", 0x8877_6655_4433_2211u64),
        ("ev_ollama_fp", 0xCAFE_BABE_DEAD_BEEFu64),
    ] {
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: ev_id.to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!(["porch"]),
                start_time: 1000.0,
                end_time: Some(1006.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.58),
                false_positive: Some(false),
                box_coords: Some(json!([0.15, 0.25, 0.35, 0.75])),
                data: json!({
                    "box": [0.15, 0.25, 0.35, 0.75],
                    "dhash64": dhash,
                    "attributes": ["color:green"]
                }),
                thumbnail: None,
                area: Some(9000),
                ratio: Some(0.45),
                score: Some(0.58),
                stationary: true,
            })
            .unwrap();
    }

    // 1. Configure Gemini provider pointing at mock_base and verify ev_gemini_fp
    {
        let mut cfg = state.config.write().await;
        cfg.genai.enabled = true;
        cfg.genai.provider = "gemini".to_string();
        cfg.genai.base_url = Some(mock_base.clone());
        cfg.genai.model = "gemini-2.5-flash".to_string();
        cfg.genai.auto_suppress_false_positives = true;
    }
    let app = build_router(state.clone());

    // Validate API & discover models across Gemini, OpenAI, and Ollama
    for (prov, expected_first) in [
        ("gemini", "gemini-2.5-flash"),
        ("openai", "gpt-4o-mini"),
        ("ollama", "qwen2.5vl:7b"),
    ] {
        let resp_models = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/genai/models")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "provider": prov,
                            "api_key": "test-key",
                            "base_url": mock_base
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_models.status(), StatusCode::OK);
        let models_json: Value =
            serde_json::from_slice(&to_bytes(resp_models.into_body(), 8192).await.unwrap())
                .unwrap();
        assert_eq!(models_json["valid"], true);
        assert_eq!(models_json["models"][0], expected_first);
    }

    let rev_gem_thumb = state
        .media_dir
        .join("clips/review/thumb-rev_gemini_fp.webp");
    std::fs::create_dir_all(rev_gem_thumb.parent().unwrap()).unwrap();
    std::fs::write(&rev_gem_thumb, b"RIFF_GEM_REV_THUMB").unwrap();
    state
        .db
        .insert_review_segment(&ninjanvr_db::ReviewItemDto {
            id: "rev_gemini_fp".to_string(),
            camera: "front_door".to_string(),
            start_time: 1700000000.0,
            end_time: Some(1700000005.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-rev_gemini_fp.webp".to_string(),
            data: json!({
                "detections": ["ev_gemini_fp"],
                "objects": ["person"]
            }),
            has_been_reviewed: false,
        })
        .unwrap();
    let mut ws_gem_rx = state.comms.subscribe_ws();

    let resp_gem = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events/ev_gemini_fp/verify-ai")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_gem.status(), StatusCode::OK);
    let gem_json: Value =
        serde_json::from_slice(&to_bytes(resp_gem.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(gem_json["is_true_positive"], false);
    assert_eq!(gem_json["false_positive"], true);
    assert_eq!(
        gem_json["ai_verification"]["reason"],
        "Static patio fern misclassified as person"
    );
    assert!(
        state
            .db
            .get_review_by_id("rev_gemini_fp")
            .unwrap()
            .is_none(),
        "auto_suppress_false_positives must reconcile and delete empty reviewsegment"
    );
    assert!(
        !rev_gem_thumb.exists(),
        "auto_suppress_false_positives must unlink deleted review thumbnail"
    );
    let ws_ev_msg = ws_gem_rx.recv().await.unwrap();
    let ws_rev_msg = ws_gem_rx.recv().await.unwrap();
    assert!(ws_ev_msg.contains("\"topic\":\"events\""));
    assert!(ws_rev_msg.contains("\"topic\":\"reviews\""));
    assert!(ws_rev_msg.contains("rev_gemini_fp"));

    // Verify event row has ai_verification persisted and false_positive = true
    let ev_after = state.db.get_event("ev_gemini_fp").unwrap().unwrap();
    assert_eq!(ev_after["false_positive"], true);
    assert_eq!(
        ev_after["data"]["ai_verification"]["is_true_positive"],
        false
    );

    // Verify GET /api/false_positives/signatures returns the auto-created signature
    let resp_sigs = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/false_positives/signatures")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_sigs.status(), StatusCode::OK);
    let sigs_json: Value =
        serde_json::from_slice(&to_bytes(resp_sigs.into_body(), 8192).await.unwrap()).unwrap();
    let sigs_arr = sigs_json.as_array().unwrap();
    assert_eq!(sigs_arr.len(), 1);
    assert_eq!(sigs_arr[0]["source_event_id"], "ev_gemini_fp");
    assert_eq!(
        sigs_arr[0]["reason"],
        "Static patio fern misclassified as person"
    );
    let sig_id = sigs_arr[0]["id"].as_i64().unwrap();

    // 2. Configure OpenAI provider pointing at mock_base and verify ev_openai_tp (true positive)
    {
        let mut cfg = state.config.write().await;
        cfg.genai.provider = "openai".to_string();
        cfg.genai.model = "gpt-4o-mini".to_string();
    }
    let resp_oai = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events/ev_openai_tp/verify-ai")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_oai.status(), StatusCode::OK);
    let oai_json: Value =
        serde_json::from_slice(&to_bytes(resp_oai.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(oai_json["is_true_positive"], true);
    assert_eq!(oai_json["false_positive"], false);

    // 3. Configure Ollama provider pointing at mock_base and verify ev_ollama_fp (false positive)
    {
        let mut cfg = state.config.write().await;
        cfg.genai.provider = "ollama".to_string();
        cfg.genai.model = "qwen2.5vl:7b".to_string();
    }
    let resp_oll = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events/ev_ollama_fp/verify-ai")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_oll.status(), StatusCode::OK);
    let oll_json: Value =
        serde_json::from_slice(&to_bytes(resp_oll.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(oll_json["is_true_positive"], false);
    assert_eq!(oll_json["false_positive"], true);

    // 4. DELETE /api/false_positives/signatures/{id} removes the signature
    let resp_del = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/false_positives/signatures/{sig_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_del.status(), StatusCode::OK);

    // 5. Verify Gemini thinking-model multi-part payload parsing (thought part + JSON answer part)
    let gemini_thinking_payload = json!({
        "candidates": [{
            "content": {
                "parts": [
                    {
                        "thought": true,
                        "text": "The user wants me to inspect the office camera image for false positives..."
                    },
                    {
                        "text": "{\"is_true_positive\": false, \"confidence\": 0.99, \"reason\": \"Empty office room; draped jacket on chair triggered false detection.\"}",
                        "thoughtSignature": "sig_test_123"
                    }
                ]
            }
        }]
    });
    let (tp, conf, reason) = parse_ai_judge_json_verdict(&gemini_thinking_payload).unwrap();
    assert!(!tp);
    assert!((conf - 0.99).abs() < 1e-6);
    assert!(reason.contains("Empty office room"));

    let gemini_thinking_annotations = json!({
        "candidates": [{
            "content": {
                "parts": [
                    {
                        "thought": true,
                        "text": "Detecting objects in frame..."
                    },
                    {
                        "text": "{\"detections\": [{\"label\": \"person\", \"bbox\": [0.1, 0.2, 0.5, 0.8], \"score\": 0.93}]}"
                    }
                ]
            }
        }]
    });
    let parsed_boxes = parse_ai_auto_annotations(&gemini_thinking_annotations, 1280, 720).unwrap();
    assert_eq!(parsed_boxes.len(), 1);
    assert_eq!(parsed_boxes[0].label, "person");

    // 6. Verify error chain includes root cause while scrubbing secret API keys
    mock_srv.abort();
    {
        let mut cfg = state.config.write().await;
        cfg.genai.provider = "gemini".to_string();
        cfg.genai.model = "gemini-3.8-flash".to_string();
        cfg.genai.api_key = Some("AIzaSySuperSecretTestKey999".to_string());
        cfg.genai.base_url = Some("http://127.0.0.1:1".to_string());
    }
    let resp_err = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events/ev_gemini_fp/verify-ai")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_err.status(), StatusCode::BAD_GATEWAY);
    let err_json: Value =
        serde_json::from_slice(&to_bytes(resp_err.into_body(), 8192).await.unwrap()).unwrap();
    let err_msg = err_json["message"].as_str().unwrap_or("");
    assert!(err_msg.contains("Gemini Vision API request failed:"));
    assert!(
        !err_msg.contains("AIzaSySuperSecretTestKey999"),
        "API key must be scrubbed from error output: {err_msg}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn onvif_ptz_soap_endpoints_handle_move_stop_preset_status_floats_faults_and_timeouts() {
    // 1. Unit-verify tolerant float parser on non-standard ONVIF float strings (#24833)
    assert!((parse_onvif_float("+0.5000000", -1.0, 1.0, 0.0) - 0.5).abs() < 1e-5);
    assert!((parse_onvif_float(" -0.2500000 ", -1.0, 1.0, 0.0) - (-0.25)).abs() < 1e-5);
    assert!((parse_onvif_float("+0.7500", 0.0, 1.0, 0.0) - 0.75).abs() < 1e-5);
    assert!((parse_onvif_float("0,4000", -1.0, 1.0, 0.0) - 0.4).abs() < 1e-5);
    assert_eq!(parse_onvif_float("NaN", -1.0, 1.0, 0.0), 0.0);
    assert_eq!(parse_onvif_float("not_a_float", -1.0, 1.0, 0.0), 0.0);
    assert_eq!(parse_onvif_float("+9.9999", -1.0, 1.0, 0.0), 1.0);

    // 2. Spin up a local mock ONVIF SOAP HTTP server
    let mode = Arc::new(std::sync::Mutex::new("ok".to_string()));
    let recorded_actions: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let mode_srv = mode.clone();
    let actions_srv = recorded_actions.clone();
    let mock_onvif_router = Router::new().route(
            "/onvif/ptz_service",
            post(move |body: String| {
                let mode_now = mode_srv.lock().unwrap().clone();
                let actions_log = actions_srv.clone();
                async move {
                    let action_tag = if body.contains("GetPresets") {
                        "GetPresets"
                    } else if body.contains("GetStatus") {
                        "GetStatus"
                    } else if body.contains("ContinuousMove") {
                        "ContinuousMove"
                    } else if body.contains("RelativeMove") {
                        "RelativeMove"
                    } else if body.contains("GotoPreset") {
                        "GotoPreset"
                    } else if body.contains("Stop") {
                        "Stop"
                    } else {
                        "Unknown"
                    };
                    actions_log.lock().unwrap().push(action_tag.to_string());

                    if mode_now == "timeout" {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    if mode_now == "fault" {
                        let fault_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body>
    <s:Fault>
      <s:Reason><s:Text xml:lang="en">PTZ Hardware Motor Busy</s:Text></s:Reason>
    </s:Fault>
  </s:Body>
</s:Envelope>"#;
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            [(header::CONTENT_TYPE, "application/soap+xml")],
                            fault_xml.to_string(),
                        );
                    }

                    let resp_xml = match action_tag {
                        "GetPresets" => r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
  <s:Body>
    <tptz:GetPresetsResponse>
      <tptz:Preset token="entry_gate">
        <tt:Name>entry_gate</tt:Name>
      </tptz:Preset>
      <tptz:Preset token="driveway">
        <tt:Name>driveway</tt:Name>
      </tptz:Preset>
    </tptz:GetPresetsResponse>
  </s:Body>
</s:Envelope>"#,
                        "GetStatus" => r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
  <s:Body>
    <tptz:GetStatusResponse>
      <tptz:PTZStatus>
        <tt:Position>
          <tt:PanTilt x="+0.5000000" y="-0.2500000"/>
          <tt:Zoom x="+0.1500000"/>
        </tt:Position>
        <tt:MoveStatus>
          <tt:PanTilt>IDLE</tt:PanTilt>
        </tt:MoveStatus>
      </tptz:PTZStatus>
    </tptz:GetStatusResponse>
  </s:Body>
</s:Envelope>"#,
                        "ContinuousMove" => r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tptz:ContinuousMoveResponse/></s:Body></s:Envelope>"#,
                        "RelativeMove" => r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tptz:RelativeMoveResponse/></s:Body></s:Envelope>"#,
                        "Stop" => r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tptz:StopResponse/></s:Body></s:Envelope>"#,
                        "GotoPreset" => r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tptz:GotoPresetResponse/></s:Body></s:Envelope>"#,
                        _ => r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body/></s:Envelope>"#,
                    };
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/soap+xml")],
                        resp_xml.to_string(),
                    )
                }
            }),
        );

    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    let mock_handle = tokio::spawn(async move {
        let _ = axum::serve(mock_listener, mock_onvif_router).await;
    });

    let ctx = setup_test_app().await;
    let app = &ctx.app;
    let state = &ctx.state;

    // Configure front_door with ONVIF pointing at mock_addr
    {
        let mut cfg_guard = state.config.write().await;
        if let Some(cam) = cfg_guard.cameras.get_mut("front_door") {
            cam.onvif = ninjanvr_config::OnvifCameraConfig {
                host: Some("127.0.0.1".to_string()),
                port: mock_addr.port(),
                user: Some("admin".to_string()),
                password: Some("onvif_pass".to_string()),
                autotracking: true,
                return_preset: Some("entry_gate".to_string()),
                timeout_sec: 2,
            };
        }
    }
    state
        .ptz_active_presets
        .write()
        .await
        .insert("front_door".to_string(), Some("entry_gate".to_string()));

    // A. GET /api/front_door/ptz/info parses GetPresets & GetStatus (+0.5000000 floats)
    let resp_info = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/ptz/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_info.status(), StatusCode::OK);
    let info_json: Value =
        serde_json::from_slice(&to_bytes(resp_info.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(info_json["enabled"], true);
    assert_eq!(info_json["online"], true);
    assert_eq!(info_json["active_preset"], "entry_gate");
    assert_eq!(info_json["preset_names"], json!(["entry_gate", "driveway"]));
    assert!((info_json["status"]["pan"].as_f64().unwrap() - 0.5).abs() < 1e-4);
    assert!((info_json["status"]["tilt"].as_f64().unwrap() - (-0.25)).abs() < 1e-4);
    assert!((info_json["status"]["zoom"].as_f64().unwrap() - 0.15).abs() < 1e-4);
    assert_eq!(info_json["status"]["move_status"], "IDLE");

    // B. POST /api/front_door/ptz/move (ContinuousMove) clears active_preset
    let resp_move = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/ptz/move")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"mode":"continuous","pan":-0.6,"tilt":0.3,"zoom":0.0}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_move.status(), StatusCode::OK);
    let move_json: Value =
        serde_json::from_slice(&to_bytes(resp_move.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(move_json["success"], true);
    assert_eq!(move_json["operation"], "ContinuousMove");
    assert!(move_json["active_preset"].is_null());

    // C. POST /api/front_door/ptz/move (RelativeMove)
    let resp_rel = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/ptz/move")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"mode":"relative","action":"zoom_in","speed":0.4}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rel.status(), StatusCode::OK);
    let rel_json: Value =
        serde_json::from_slice(&to_bytes(resp_rel.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(rel_json["success"], true);
    assert_eq!(rel_json["operation"], "RelativeMove");

    // D. POST /api/front_door/ptz/stop
    let resp_stop = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/ptz/stop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_stop.status(), StatusCode::OK);
    let stop_json: Value =
        serde_json::from_slice(&to_bytes(resp_stop.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(stop_json["success"], true);
    assert_eq!(stop_json["operation"], "Stop");

    // E. POST /api/front_door/ptz/preset (GotoPreset) restores active_preset to "driveway"
    let resp_preset = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/ptz/preset")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"preset":"driveway"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_preset.status(), StatusCode::OK);
    let preset_json: Value =
        serde_json::from_slice(&to_bytes(resp_preset.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(preset_json["success"], true);
    assert_eq!(preset_json["operation"], "GotoPreset");
    assert_eq!(preset_json["active_preset"], "driveway");
    assert_eq!(
        state
            .ptz_active_presets
            .read()
            .await
            .get("front_door")
            .and_then(|p| p.as_deref()),
        Some("driveway")
    );

    // F. SOAP Fault handling: mock server returns 500 SOAP Fault -> 502 Bad Gateway
    *mode.lock().unwrap() = "fault".to_string();
    let resp_fault = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/ptz/move")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"action":"left"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_fault.status(), StatusCode::BAD_GATEWAY);
    let fault_json: Value =
        serde_json::from_slice(&to_bytes(resp_fault.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(fault_json["success"], false);
    assert!(
        fault_json["error"]
            .as_str()
            .unwrap_or("")
            .contains("PTZ Hardware Motor Busy")
    );

    // G. Strict Timeout handling: mock server delays 200ms while timeout_ms is 30ms -> 504 Gateway Timeout
    *mode.lock().unwrap() = "timeout".to_string();
    let resp_timeout = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/ptz/move")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"mode":"relative","pan":0.1,"tilt":0.0,"timeout_ms":30}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_timeout.status(), StatusCode::GATEWAY_TIMEOUT);

    // H. Recovery after dropped/timed-out RelativeMove packet (#24834)
    *mode.lock().unwrap() = "ok".to_string();
    let resp_recover = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/ptz/move")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"mode":"relative","pan":0.15,"tilt":-0.05}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_recover.status(), StatusCode::OK);

    let actions = recorded_actions.lock().unwrap().clone();
    assert!(actions.contains(&"GetPresets".to_string()));
    assert!(actions.contains(&"GetStatus".to_string()));
    assert!(actions.contains(&"ContinuousMove".to_string()));
    assert!(actions.contains(&"RelativeMove".to_string()));
    assert!(actions.contains(&"Stop".to_string()));
    assert!(actions.contains(&"GotoPreset".to_string()));

    mock_handle.abort();
}

#[tokio::test]
async fn face_recognition_gallery_and_sub_label_http_endpoints_work_end_to_end() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr_faces_http_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();

    let cfg_path = tmp_dir.join("config.yml");
    let db_path = tmp_dir.join("ninjanvr.db");
    std::fs::write(
        &cfg_path,
        format!(
            "database:\n  path: {}\nversion: 0.18-0\n",
            db_path.display()
        ),
    )
    .unwrap();
    let state = new_app_state(&cfg_path, tmp_dir.join("web"), tmp_dir.join("media"), false)
        .await
        .unwrap();

    // Seed a person event and reviewsegment
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_person_bruno".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!(["porch"]),
            start_time: 1000.0,
            end_time: Some(1004.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.93),
            false_positive: Some(false),
            box_coords: Some(json!([0.1, 0.1, 0.4, 0.8])),
            data: json!({ "box": [0.1, 0.1, 0.4, 0.8] }),
            thumbnail: None,
            area: Some(15000),
            ratio: Some(0.5),
            score: Some(0.93),
            stationary: false,
        })
        .unwrap();

    let app = build_router(state.clone());

    // 1. POST /api/faces/Bruno/enroll
    let enroll_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/faces/Bruno/enroll")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"image_base64":"ZmFrZS1mYWNlLWNyb3AtYnJ1bm8="}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(enroll_resp.status(), StatusCode::OK);
    let enroll_json: Value =
        serde_json::from_slice(&to_bytes(enroll_resp.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(enroll_json["success"], true);
    assert_eq!(enroll_json["name"], "Bruno");
    assert_eq!(enroll_json["sample_count"], 1);
    assert_eq!(enroll_json["embedding_dim"], 512);

    // 2. GET /api/faces
    let list_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/faces")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_json: Value =
        serde_json::from_slice(&to_bytes(list_resp.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(list_json["profiles"][0]["name"], "Bruno");
    assert_eq!(list_json["profiles"][0]["sample_count"], 1);
    assert!(list_json["faces"]["Bruno"].is_array());

    // 3. POST /api/events/{id}/sub_label
    let sub_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events/ev_person_bruno/sub_label")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"sub_label":"Bruno","sub_label_score":0.89}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sub_resp.status(), StatusCode::OK);
    let ev_after = state.db.get_event("ev_person_bruno").unwrap().unwrap();
    assert_eq!(ev_after["sub_label"], "Bruno");
    assert!((ev_after["sub_label_score"].as_f64().unwrap() - 0.89).abs() < 1e-5);

    // 4. DELETE /api/faces/Bruno
    let del_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/faces/Bruno")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_resp.status(), StatusCode::OK);
    let del_json: Value =
        serde_json::from_slice(&to_bytes(del_resp.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(del_json["success"], true);
    assert_eq!(del_json["deleted"], true);
    assert_eq!(del_json["cleared_events"], 1);

    let ev_cleared = state.db.get_event("ev_person_bruno").unwrap().unwrap();
    assert!(ev_cleared["sub_label"].is_null());
    assert!(ev_cleared["sub_label_score"].is_null());

    let list_after = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/faces")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_after_json: Value =
        serde_json::from_slice(&to_bytes(list_after.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(list_after_json["profiles"].as_array().unwrap().len(), 0);

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn dataset_from_event_auto_annotate_and_zip_export_end_to_end() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-dataset-e2e-{}", rand::random::<u32>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("config.yml");
    let db_path = tmp_dir.join("ninjanvr.db");
    std::fs::write(
        &cfg_path,
        format!(
            "database:\n  path: {}\nversion: 0.18-0\n",
            db_path.display()
        ),
    )
    .unwrap();

    let media_dir = tmp_dir.join("media");
    let clips_dir = media_dir.join("clips");
    std::fs::create_dir_all(&clips_dir).unwrap();

    let state = new_app_state(&cfg_path, tmp_dir.join("web"), media_dir.clone(), false)
        .await
        .unwrap();

    //Spin up a local mock Vision AI endpoint returning normalized bounding boxes
    let mock_router = Router::new()
            .route(
                "/v1beta/models/{*model_action}",
                post(|| async {
                    Json(json!({
                        "candidates": [{
                            "content": {
                                "parts": [{
                                    "text": "{\"detections\": [{\"label\": \"person\", \"bbox\": [0.10, 0.20, 0.50, 0.80], \"score\": 0.97}, {\"label\": \"package\", \"bbox\": [0.55, 0.70, 0.75, 0.90], \"score\": 0.93}]}"
                                }]
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/v1/chat/completions",
                post(|| async {
                    Json(json!({
                        "choices": [{
                            "message": {
                                "content": "{\"annotations\": [{\"label\": \"car\", \"box_2d\": [200, 100, 800, 900], \"confidence\": 0.94}]}"
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/api/generate",
                post(|| async {
                    Json(json!({
                        "response": "{\"boxes\": [{\"class_id\": 7, \"label\": \"license_plate\", \"x_min\": 0.40, \"y_min\": 0.60, \"x_max\": 0.60, \"y_max\": 0.70, \"score\": 0.91}]}"
                    }))
                }),
            );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    let mock_base = format!("http://{mock_addr}");
    let mock_srv = tokio::spawn(async move {
        let _ = axum::serve(mock_listener, mock_router).await;
    });

    {
        let mut cfg = state.config.write().await;
        cfg.genai.enabled = true;
        cfg.genai.provider = "gemini".to_string();
        cfg.genai.base_url = Some(mock_base);
        cfg.genai.model = "gemini-2.5-flash".to_string();
    }

    // Write clean snapshots to disk and insert events
    let img = image::RgbImage::from_pixel(64, 36, image::Rgb([64, 64, 64]));
    let webp_bytes = ninjanvr_vision::encode_image_webp(&img).unwrap();
    std::fs::write(clips_dir.join("front_door-ev_tp_1-clean.webp"), &webp_bytes).unwrap();
    std::fs::write(clips_dir.join("patio-ev_fp_1-clean.webp"), &webp_bytes).unwrap();

    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_tp_1".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!(["porch"]),
            start_time: 2000.0,
            end_time: Some(2005.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.91),
            false_positive: Some(false),
            box_coords: Some(json!([0.20, 0.10, 0.60, 0.90])),
            data: json!({ "box": [0.20, 0.10, 0.60, 0.90] }),
            thumbnail: None,
            area: Some(10000),
            ratio: Some(0.5),
            score: Some(0.91),
            stationary: false,
        })
        .unwrap();

    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_fp_1".to_string(),
            camera: "patio".to_string(),
            label: "person".to_string(),
            zones: json!(["yard"]),
            start_time: 2010.0,
            end_time: Some(2014.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.62),
            false_positive: Some(false),
            box_coords: Some(json!([0.10, 0.10, 0.30, 0.50])),
            data: json!({ "box": [0.10, 0.10, 0.30, 0.50] }),
            thumbnail: None,
            area: Some(6000),
            ratio: Some(0.5),
            score: Some(0.62),
            stationary: true,
        })
        .unwrap();

    let app = build_router(state.clone());

    // 1. POST /api/dataset/samples/from-event/ev_tp_1 creates true_positive sample
    let resp_from_ev = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/dataset/samples/from-event/ev_tp_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_from_ev.status(), StatusCode::OK);
    let tp_json: Value =
        serde_json::from_slice(&to_bytes(resp_from_ev.into_body(), 16384).await.unwrap()).unwrap();
    let tp_sample_id = tp_json["id"].as_str().unwrap().to_string();
    assert_eq!(tp_sample_id, "ds_ev_tp_1");
    assert_eq!(tp_json["sample_kind"], "true_positive");
    assert_eq!(tp_json["annotations"][0]["class_id"], 0);
    assert_eq!(tp_json["annotations"][0]["cx"], 0.4);
    assert_eq!(tp_json["annotations"][0]["cy"], 0.5);
    assert_eq!(tp_json["annotations"][0]["w"], 0.4);
    assert_eq!(tp_json["annotations"][0]["h"], 0.8);
    assert!(media_dir.join("dataset/ds_ev_tp_1.webp").exists());

    // 2. PUT /api/events/ev_fp_1/false_positive creates hard_negative sample with empty annotations
    let resp_fp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/events/ev_fp_1/false_positive")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"false_positive":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_fp.status(), StatusCode::OK);
    let hn_sample = state
        .db
        .get_dataset_sample("ds_ev_fp_1")
        .unwrap()
        .expect("hard_negative sample created on false_positive");
    assert_eq!(hn_sample.sample_kind, "hard_negative");
    assert!(hn_sample.annotations.is_empty());
    assert!(media_dir.join("dataset/ds_ev_fp_1.webp").exists());

    // 3. POST /api/dataset/samples/ds_ev_tp_1/auto-annotate invokes Teacher AI & converts boxes to YOLO
    let resp_auto = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/dataset/samples/ds_ev_tp_1/auto-annotate")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_auto.status(), StatusCode::OK);
    let auto_json: Value =
        serde_json::from_slice(&to_bytes(resp_auto.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(auto_json["sample_kind"], "ai_annotated");
    // Teacher output is verified on the spot and attributed to the AI.
    assert_eq!(auto_json["verified"], true);
    assert_eq!(auto_json["verified_by"], "ai");
    let anns = auto_json["annotations"].as_array().unwrap();
    assert_eq!(anns.len(), 2);
    // [0.10, 0.20, 0.50, 0.80] -> cx=0.30, cy=0.50, w=0.40, h=0.60
    assert_eq!(anns[0]["class_id"], 0);
    assert_eq!(anns[0]["label"], "person");
    assert_eq!(anns[0]["cx"], 0.3);
    assert_eq!(anns[0]["cy"], 0.5);
    assert_eq!(anns[0]["w"], 0.4);
    assert_eq!(anns[0]["h"], 0.6);
    // [0.55, 0.70, 0.75, 0.90] -> cx=0.65, cy=0.80, w=0.20, h=0.20
    assert_eq!(anns[1]["class_id"], 11);
    assert_eq!(anns[1]["label"], "package");
    assert_eq!(anns[1]["cx"], 0.65);
    assert_eq!(anns[1]["cy"], 0.8);
    assert_eq!(anns[1]["w"], 0.2);
    assert_eq!(anns[1]["h"], 0.2);

    // Pin both samples to 'train' split and verified=true via PUT /api/dataset/samples/{id}
    for sid in ["ds_ev_tp_1", "ds_ev_fp_1"] {
        let resp_put = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/dataset/samples/{sid}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"verified":true,"split":"train"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_put.status(), StatusCode::OK);
    }

    // Verify GET /api/dataset/summary and GET /api/dataset/samples/{id}/image.webp
    let resp_sum = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/dataset/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_sum.status(), StatusCode::OK);
    let sum_json: Value =
        serde_json::from_slice(&to_bytes(resp_sum.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(sum_json["total_samples"], 2);
    assert_eq!(sum_json["hard_negative_count"], 1);
    assert_eq!(sum_json["ai_annotated_count"], 1);
    // The explicit PUT {"verified":true} above is an operator confirmation, so the
    // AI-verified sample is now attributed to a person; the hard negative was
    // operator-made from the start.
    assert_eq!(sum_json["ai_verified_count"], 0);
    assert_eq!(sum_json["manual_verified_count"], 1);
    assert_eq!(sum_json["pending_count"], 0);

    let resp_img = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/dataset/samples/ds_ev_tp_1/image.webp")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_img.status(), StatusCode::OK);

    // 4. GET /api/dataset/export.zip -> parse ZIP archive and assert contents
    let resp_zip = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/dataset/export.zip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_zip.status(), StatusCode::OK);
    assert_eq!(
        resp_zip.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/zip"
    );
    let zip_bytes = to_bytes(resp_zip.into_body(), 1024 * 1024).await.unwrap();
    let archive = parse_zip_archive(&zip_bytes).expect("valid ZIP archive");

    let data_yaml = String::from_utf8(archive.get("data.yaml").unwrap().clone()).unwrap();
    assert!(data_yaml.contains("train: images/train"));
    assert!(data_yaml.contains("val: images/val"));
    assert!(data_yaml.contains("nc: 15"));
    assert!(data_yaml.contains("0: person"));
    assert!(data_yaml.contains("11: package"));
    assert!(data_yaml.contains("14: umbrella"));

    assert!(archive.contains_key("images/train/ds_ev_tp_1.webp"));
    assert!(archive.contains_key("images/train/ds_ev_fp_1.webp"));

    let tp_label_txt =
        String::from_utf8(archive.get("labels/train/ds_ev_tp_1.txt").unwrap().clone()).unwrap();
    assert_eq!(
        tp_label_txt.trim(),
        "0 0.300000 0.500000 0.400000 0.600000\n11 0.650000 0.800000 0.200000 0.200000"
    );

    let hn_label_txt =
        String::from_utf8(archive.get("labels/train/ds_ev_fp_1.txt").unwrap().clone()).unwrap();
    assert!(
        hn_label_txt.is_empty(),
        "hard negative sample must produce an empty .txt label file"
    );

    // 5. Verify GET /api/dataset/samples, GET /api/dataset/samples/{id}, OpenAI + Ollama batch auto-annotate, and DELETE
    let resp_list = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/dataset/samples?camera=front_door")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_list.status(), StatusCode::OK);
    let list_json: Value =
        serde_json::from_slice(&to_bytes(resp_list.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(list_json.as_array().unwrap().len(), 1);

    let resp_one = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/dataset/samples/ds_ev_tp_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_one.status(), StatusCode::OK);

    // Mark ds_ev_tp_1 unverified and test batch auto-annotate with OpenAI & Ollama providers
    state
        .db
        .update_dataset_sample_annotations("ds_ev_tp_1", None, None, Some(false), None, None)
        .unwrap();
    {
        let mut cfg = state.config.write().await;
        cfg.genai.provider = "openai".to_string();
        cfg.genai.model = "gpt-4o-mini".to_string();
    }
    let resp_batch_oai = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/dataset/auto-annotate")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_batch_oai.status(), StatusCode::OK);

    {
        let mut cfg = state.config.write().await;
        cfg.genai.provider = "ollama".to_string();
        cfg.genai.model = "qwen2.5vl:7b".to_string();
    }
    let resp_batch_oll = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/dataset/auto-annotate")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_batch_oll.status(), StatusCode::OK);

    let resp_del = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/dataset/samples/ds_ev_fp_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_del.status(), StatusCode::OK);

    mock_srv.abort();
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn detector_inference_metrics_export_only_measured_latencies() {
    let stats = json!({
        "primary_detector": { "inference_speed": 85.8, "measured": true },
        "detectors": {
            "detector01": { "inference_speed": 0.0, "measured": false, "in_use": false },
            "garage_gpu": { "inference_speed": 12.5, "measured": true, "in_use": true },
            "legacy": { "inference_speed": 7.0 }
        }
    });
    let mut out = String::new();
    render_detector_inference_metrics(&stats, &mut out);
    assert!(out.contains("ninjanvr_detector_inference_ms{detector=\"primary\"} 85.80\n"));
    assert!(out.contains("ninjanvr_detector_inference_ms{detector=\"garage_gpu\"} 12.50\n"));
    assert!(!out.contains("detector01"));
    // Stats from servers without the "measured" flag keep their previous behaviour.
    assert!(out.contains("ninjanvr_detector_inference_ms{detector=\"legacy\"} 7.00\n"));

    let mut idle = String::new();
    render_detector_inference_metrics(
        &json!({ "primary_detector": { "inference_speed": 0.0, "measured": false } }),
        &mut idle,
    );
    assert!(idle.is_empty());
}

#[test]
fn model_deploy_rolls_back_only_when_it_would_break_a_working_detector() {
    assert!(should_rollback_model_deploy(true, false));
    assert!(!should_rollback_model_deploy(true, true));
    assert!(!should_rollback_model_deploy(false, true));
    assert!(!should_rollback_model_deploy(false, false));
}

#[tokio::test]
async fn detector_model_upload_persists_and_hot_reloads() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-model-upload-{}", rand::random::<u32>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("config.yml");
    let db_path = tmp_dir.join("ninjanvr.db");
    std::fs::write(
        &cfg_path,
        format!(
            "version: 0.18-0\ndatabase:\n  path: {}\n",
            db_path.display()
        ),
    )
    .unwrap();

    let media_dir = tmp_dir.join("media");
    let state = new_app_state(&cfg_path, tmp_dir.join("web"), media_dir.clone(), false)
        .await
        .unwrap();
    let app = build_router(state.clone());

    let synthetic_onnx =
        base64::engine::general_purpose::STANDARD.encode(b"ONNX_FINE_TUNED_YOLO11S_WEIGHTS");
    let upload_body = json!({
        "filename": "ninjanvr_custom_yolo11s.onnx",
        "model_base64": synthetic_onnx,
        "model_type": "yolo-generic",
        "width": 640,
        "height": 640,
        "input_tensor": "nchw",
        "input_pixel_format": "rgb"
    });

    let resp_upload = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/detectors/models/upload")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&upload_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_upload.status(), StatusCode::OK);
    let upload_json: Value =
        serde_json::from_slice(&to_bytes(resp_upload.into_body(), 32768).await.unwrap()).unwrap();
    assert_eq!(upload_json["success"], true);
    assert_eq!(upload_json["reloaded"], true);
    assert_eq!(upload_json["filename"], "ninjanvr_custom_yolo11s.onnx");
    // No OpenVINO runtime in tests: the response must say so instead of claiming success.
    assert_eq!(upload_json["loaded"], false);
    assert_eq!(upload_json["persisted"], true);
    assert!(upload_json["deployed_at"].as_u64().unwrap_or(0) > 0);

    // The deploy must survive a restart: config.yml on disk points at the new model.
    let on_disk = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(
        on_disk.contains("ninjanvr_custom_yolo11s.onnx"),
        "config.yml must be updated by the upload, got:\n{on_disk}"
    );
    let reparsed = ninjanvr_config::NinjaConfig::load(&cfg_path).unwrap();
    assert!(
        reparsed
            .model
            .path
            .ends_with("models/ninjanvr_custom_yolo11s.onnx")
    );
    assert!(
        reparsed
            .model
            .labelmap_path
            .ends_with("models/ninjanvr_custom_yolo11s.txt")
    );
    assert_eq!((reparsed.model.width, reparsed.model.height), (640, 640));
    assert!(
        state
            .config
            .read()
            .await
            .model
            .path
            .ends_with("ninjanvr_custom_yolo11s.onnx")
    );

    // Verify model and 15-class labelmap files were persisted in {media_dir}/models/
    let expected_model_file = media_dir.join("models/ninjanvr_custom_yolo11s.onnx");
    let expected_labelmap_file = media_dir.join("models/ninjanvr_custom_yolo11s.txt");
    assert!(expected_model_file.exists());
    assert!(expected_labelmap_file.exists());
    assert_eq!(
        std::fs::read(&expected_model_file).unwrap(),
        b"ONNX_FINE_TUNED_YOLO11S_WEIGHTS"
    );
    let labelmap_contents = std::fs::read_to_string(&expected_labelmap_file).unwrap();
    assert!(labelmap_contents.contains("person\nface\ncar"));
    assert_eq!(labelmap_contents.lines().count(), 15);

    // Verify AiDetectorEngine was hot-reloaded with the new dimensions (640x640)
    assert_eq!(state.detector.model_dimensions(), (640, 640, false));

    // Verify GET /api/detectors/catalog discovers the newly uploaded model
    let resp_cat = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/detectors/catalog")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_cat.status(), StatusCode::OK);
    let cat_json: Value =
        serde_json::from_slice(&to_bytes(resp_cat.into_body(), 32768).await.unwrap()).unwrap();
    let discovered = cat_json["discovered_models"].as_array().unwrap();
    assert!(
        discovered.iter().any(|m| m["path"]
            .as_str()
            .unwrap_or("")
            .ends_with("ninjanvr_custom_yolo11s.onnx")),
        "GET /api/detectors/catalog must list uploaded model in discovered_models"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn evaluate_system_heuristics_detects_duplicate_rtsp_aspect_cpu_storage_retention_audio_and_quality_drift()
 {
    let yaml = r#"
go2rtc:
  streams:
    cam_a_main: "rtsp://admin:pass1@192.0.2.10:554/stream1#video=copy"
    cam_a_dup: "rtsp://viewer:pass2@192.0.2.10:554/stream1"
    cam_local: "rtsp://127.0.0.1:8554/cam_a_main"
cameras:
  cam_a:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/cam_a_main
          roles: [record, detect, audio]
    detect:
      width: 640
      height: 480
    record:
      enabled: true
      continuous:
        days: 7
      preview:
        quality: very_high
    snapshots:
      quality: 95
  cam_b:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/cam_b_main
          roles: [record, detect]
    detect:
      width: 1280
      height: 720
    record:
      enabled: true
      continuous:
        days: 2
      preview:
        quality: medium
    snapshots:
      quality: 100
"#;
    let cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
    let mut cam_stats = serde_json::Map::new();
    cam_stats.insert("cam_a".to_string(), json!({ "ffmpeg_cpu": "3.6" }));
    cam_stats.insert("cam_b".to_string(), json!({ "ffmpeg_cpu": "0.5" }));

    let rec_mount = json!({
        "total": 100_000.0,
        "used": 96_000.0,
        "free": 4_000.0
    });

    let alerts = evaluate_system_heuristics(&cfg, &cam_stats, &rec_mount, 1.5);
    let ids: Vec<&str> = alerts.iter().map(|a| a.id.as_str()).collect();
    assert!(
        ids.contains(&"duplicate_rtsp_producer_cam_a_dup")
            || ids.contains(&"duplicate_rtsp_producer_cam_a_main")
    );
    assert!(ids.contains(&"detect_aspect_mismatch_cam_a"));
    assert!(!ids.contains(&"detect_aspect_mismatch_cam_b"));
    assert!(ids.contains(&"detect_ffmpeg_cpu_cam_a"));
    assert!(ids.contains(&"storage_capacity_unbounded"));
    assert!(ids.contains(&"retention_runway_asymmetry"));
    assert!(ids.contains(&"audio_role_disabled_mismatch"));
    assert!(ids.contains(&"camera_quality_drift"));

    let storage_alert = alerts
        .iter()
        .find(|a| a.id == "storage_capacity_unbounded")
        .unwrap();
    assert_eq!(storage_alert.severity, "critical");

    let log_hub = LogBufferHub::new();
    emit_heuristics_to_logs(&log_hub, &alerts);
    let (total_logged, logged) = log_hub.query_lines("ninjanvr", None, None);
    assert_eq!(total_logged, alerts.len());
    assert_eq!(logged.len(), alerts.len());
    assert!(
        logged
            .iter()
            .any(|l| l.contains("ERROR") && l.contains("storage_capacity_unbounded"))
    );

    // Shared main-stream loopback camera (e.g. 'sala' downscaling 1080p -> 720p at ~2.5% CPU)
    // must not trigger a false-positive detect_ffmpeg_cpu alert and must alias sala_sub -> sala.
    let shared_yaml = r#"
version: 0.18-0
go2rtc:
  streams:
    cozinha: "rtsp://admin:pass@192.0.2.20:554/stream1"
    cozinha_sub: "rtsp://admin:pass@192.0.2.20:554/stream2"
    sala: "rtsp://admin:pass@192.0.2.21:554/cam/realmonitor?channel=1&subtype=0"
    sala_sub: "rtsp://127.0.0.1:8554/sala"
cameras:
  cozinha:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/cozinha_sub
          roles: [detect]
        - path: rtsp://127.0.0.1:8554/cozinha
          roles: [record]
  sala:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/sala
          roles: [detect, record, audio]
"#;
    let shared_cfg =
        NinjaConfig::from_yaml_str(shared_yaml, PathBuf::from("/tmp/config.yml")).unwrap();
    assert!(!shared_cfg.camera_uses_shared_main_loopback("cozinha"));
    assert!(shared_cfg.camera_uses_shared_main_loopback("sala"));
    assert_eq!(
        shared_cfg.resolve_mse_stream_alias("cozinha_sub"),
        "cozinha_sub"
    );
    assert_eq!(shared_cfg.resolve_mse_stream_alias("sala_sub"), "sala");

    let mut shared_stats = serde_json::Map::new();
    shared_stats.insert("cozinha".to_string(), json!({ "ffmpeg_cpu": "0.9" }));
    shared_stats.insert("sala".to_string(), json!({ "ffmpeg_cpu": "2.5" }));
    let healthy_mount = json!({ "total": 100_000.0, "used": 40_000.0, "free": 60_000.0 });
    let shared_alerts =
        evaluate_system_heuristics(&shared_cfg, &shared_stats, &healthy_mount, 14.0);
    assert!(
        !shared_alerts
            .iter()
            .any(|a| a.id == "detect_ffmpeg_cpu_sala"),
        "shared main-stream loopback camera at 2.5% CPU must not trigger detect_ffmpeg_cpu_sala"
    );
}

#[test]
fn crc32_and_zip_archive_roundtrip_and_corruption_detection() {
    // IEEE 802.3 standard check value for "123456789"
    assert_eq!(crc32_ieee(b""), 0x0000_0000);
    assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);

    let entries = vec![
        (
            "data.yaml".to_string(),
            b"nc: 15\nnames: [person]\n".to_vec(),
        ),
        (
            "labels/train/sample1.txt".to_string(),
            b"0 0.5 0.5 0.2 0.4\n".to_vec(),
        ),
    ];
    let mut archive = build_zip_archive(&entries);
    let parsed = parse_zip_archive(&archive).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed["data.yaml"], b"nc: 15\nnames: [person]\n");
    assert_eq!(parsed["labels/train/sample1.txt"], b"0 0.5 0.5 0.2 0.4\n");

    // Reject too-small buffer and missing EOCD
    assert!(parse_zip_archive(b"short").is_err());
    assert!(parse_zip_archive(&[0u8; 32]).is_err());

    // Corrupt a payload byte inside the first file -> must fail with CRC32 mismatch
    let first_data_offset = 30 + "data.yaml".len();
    archive[first_data_offset] ^= 0xFF;
    let err = parse_zip_archive(&archive).unwrap_err().to_string();
    assert!(err.contains("CRC32 mismatch"), "unexpected error: {err}");
}

#[test]
fn onvif_endpoint_resolver_soap_builders_fault_and_preset_parsers_and_ptz_vectors() {
    let mut onvif = ninjanvr_config::OnvifCameraConfig::default();
    assert_eq!(resolve_onvif_ptz_endpoint(&onvif), None);

    onvif.host = Some("192.0.2.25".to_string());
    onvif.port = 8080;
    assert_eq!(
        resolve_onvif_ptz_endpoint(&onvif).as_deref(),
        Some("http://192.0.2.25:8080/onvif/ptz_service")
    );

    onvif.host = Some("192.0.2.25:8899".to_string());
    assert_eq!(
        resolve_onvif_ptz_endpoint(&onvif).as_deref(),
        Some("http://192.0.2.25:8899/onvif/ptz_service")
    );

    onvif.host = Some("[::1]".to_string());
    onvif.port = 80;
    assert_eq!(
        resolve_onvif_ptz_endpoint(&onvif).as_deref(),
        Some("http://[::1]:80/onvif/ptz_service")
    );

    onvif.host = Some("https://cam.example.invalid/custom/ptz/".to_string());
    assert_eq!(
        resolve_onvif_ptz_endpoint(&onvif).as_deref(),
        Some("https://cam.example.invalid/custom/ptz")
    );

    // XML escaping in SOAP envelope credentials and anonymous header when user is None
    let anon_soap = build_onvif_stop_soap("Profile_1", true, false, None, None);
    assert!(anon_soap.contains("<s:Header/>"));
    let auth_soap = build_onvif_goto_preset_soap(
        "Profile&1",
        "Preset<Home>",
        Some(2.5),
        Some("admin"),
        Some("p&w<\"'"),
    );
    assert!(auth_soap.contains("<wsse:Username>admin</wsse:Username>"));
    assert!(auth_soap.contains("<wsse:Password>p&amp;w&lt;&quot;&apos;</wsse:Password>"));
    assert!(auth_soap.contains("<tptz:ProfileToken>Profile&amp;1</tptz:ProfileToken>"));
    assert!(auth_soap.contains("<tptz:PresetToken>Preset&lt;Home&gt;</tptz:PresetToken>"));
    assert!(auth_soap.contains("<tt:PanTilt x=\"1.0000\" y=\"1.0000\"/>"));

    // PtzMoveBody directional vectors and speed clamping
    let move_ul = PtzMoveBody {
        action: Some("up_left".to_string()),
        direction: None,
        mode: Some(" relative ".to_string()),
        pan: None,
        tilt: None,
        zoom: None,
        speed: Some(0.4),
        timeout_sec: None,
        timeout_ms: None,
        profile_token: None,
    };
    assert_eq!(move_ul.resolve_vectors(), (-0.4, 0.4, 0.0));
    assert!(move_ul.is_relative());

    // SOAP fault extraction and preset XML parsing (both self-closing and nested tags)
    assert_eq!(
        extract_onvif_soap_fault("<s:Envelope><s:Body><Ok/></s:Body></s:Envelope>"),
        None
    );
    assert_eq!(
        extract_onvif_soap_fault(
            "<s:Fault><faultstring>ActionNotSupported</faultstring></s:Fault>"
        )
        .as_deref(),
        Some("ActionNotSupported")
    );
    let presets_xml = r#"
        <tptz:GetPresetsResponse>
            <tptz:Preset token="p1"><tt:Name>Front Gate</tt:Name></tptz:Preset>
            <tptz:Preset token="p2"/>
            <Preset><token>p3</token><Name>Driveway</Name></Preset>
        </tptz:GetPresetsResponse>
        "#;
    let presets = parse_onvif_presets_xml(presets_xml);
    assert_eq!(presets.len(), 3);
    assert_eq!(presets[0].token, "p1");
    assert_eq!(presets[0].name, "Front Gate");
    assert_eq!(presets[1].token, "p2");
    assert_eq!(presets[1].name, "p2");
    assert_eq!(presets[2].token, "p3");
    assert_eq!(presets[2].name, "Driveway");
}

#[test]
fn yolo_coordinate_normalization_and_cidr_ipv6_matching_edge_cases() {
    // Reversed and out-of-bounds coordinates are clamped and converted to (cx, cy, w, h)
    let (cx, cy, w, h) = xyxy_to_yolo_normalized(0.8, 0.9, 0.2, 0.1);
    assert_eq!((cx, cy, w, h), (0.5, 0.5, 0.6, 0.8));

    // Zero-area point box clamps width/height to 0.001
    let (_, _, zw, zh) = xyxy_to_yolo_normalized(0.5, 0.5, 0.5, 0.5);
    assert_eq!((zw, zh), (0.001, 0.001));

    // event_box_to_yolo_normalized: all-zero fallback and [x, y, w, h] branch when b[2] <= b[0]
    assert_eq!(
        event_box_to_yolo_normalized([0.0, 0.0, 0.0, 0.0]),
        (0.5, 0.5, 0.25, 0.4)
    );
    assert_eq!(
        event_box_to_yolo_normalized([0.6, 0.5, 0.2, 0.3]),
        (0.7, 0.65, 0.2, 0.3)
    );

    // IPv6 CIDR, /0 wildcard, bare IP, and invalid prefix handling
    assert!(ip_matches_cidr("2001:db8::dead:beef", "2001:db8::/32"));
    assert!(!ip_matches_cidr("2001:db9::1", "2001:db8::/32"));
    assert!(ip_matches_cidr("::1", "::/0"));
    assert!(ip_matches_cidr("198.51.100.7", "0.0.0.0/0"));
    assert!(ip_matches_cidr("198.51.100.7", "198.51.100.7"));
    assert!(!ip_matches_cidr("198.51.100.7", "198.51.100.0/33"));
    assert!(!ip_matches_cidr("::1", "::1/129"));
    assert!(!ip_matches_cidr("198.51.100.7", "2001:db8::/32"));

    // scope_cameras_filter intersection and empty-intersection sentinel
    assert_eq!(
        scope_cameras_filter(
            Some(vec!["cam1".to_string(), "cam2".to_string()]),
            Some(vec!["cam2".to_string(), "cam3".to_string()])
        ),
        Some(vec!["cam2".to_string()])
    );
    assert_eq!(
        scope_cameras_filter(
            Some(vec!["cam1".to_string()]),
            Some(vec!["cam2".to_string()])
        ),
        Some(vec!["__none__".to_string()])
    );
    assert_eq!(
        scope_cameras_filter(None, Some(vec![])),
        Some(vec!["__none__".to_string()])
    );
}

#[tokio::test]
async fn server_bugfixes_case_persistence_camera_scoped_global_routes_vod_clip_and_oidc_ready() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-srv-bugfixes-{}", rand::random::<u32>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("config.yml");
    let db_path = tmp_dir.join("ninjanvr.db");
    let yaml = format!(
        r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
  oidc:
    enabled: true
    issuer_url: ""
    client_id: ""
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, tmp_dir.join("web"), tmp_dir.join("media"), false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("scoped_op", "pass1234", "operator", "front_door", 1_000)
        .unwrap();
    let app = build_router_with_auth(state.clone(), true);

    // 1. /api/auth/providers must report oidc.enabled == false when issuer_url / client_id are blank
    let resp_prov = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/providers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_prov.status(), StatusCode::OK);
    let prov_json: Value =
        serde_json::from_slice(&to_bytes(resp_prov.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(prov_json["oidc"]["enabled"], false);

    // 2. Create a camera-scoped operator token restricted to "front_door"
    let now = chrono::Utc::now().timestamp() as u64;
    let scoped_claims = JwtClaims {
        sub: "scoped_op".to_string(),
        role: "operator".to_string(),
        allowed_cameras: Some("front_door".to_string()),
        iat: now,
        exp: now + 3600,
    };
    let scoped_jwt = jsonwebtoken::encode(
        &Header::default(),
        &scoped_claims,
        &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .unwrap();
    let auth_hdr = format!("Bearer {scoped_jwt}");

    // Camera-scoped user can access global endpoints (/api/sub_labels, /api/recognized_license_plates, /api/faces)
    // and their permitted camera's /vod/clip/front_door/... route
    for uri in [
        "/api/sub_labels",
        "/api/recognized_license_plates",
        "/api/faces",
        "/vod/clip/front_door/start/100/end/200/master.m3u8",
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::AUTHORIZATION, &auth_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "camera-scoped user must be permitted to access {uri}"
        );
    }

    // Camera-scoped user is blocked from /vod/clip/backyard/... (403 Forbidden)
    let resp_forbidden = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/vod/clip/backyard/start/100/end/200/master.m3u8")
                .header(header::AUTHORIZATION, &auth_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_forbidden.status(), StatusCode::FORBIDDEN);

    // 3. POST /api/cases persists case to SQLite and GET /api/cases returns it
    let case_payload = json!({
        "id": "case_incident_42",
        "name": "Perimeter Breach",
        "description": "Night motion near gate",
        "event_ids": ["ev_101"],
        "export_ids": ["exp_202"]
    });
    let resp_case_post = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/cases")
                .header(header::AUTHORIZATION, &auth_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&case_payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_case_post.status(), StatusCode::OK);

    let resp_cases_get = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/cases")
                .header(header::AUTHORIZATION, &auth_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_cases_get.status(), StatusCode::OK);
    let cases_list: Value =
        serde_json::from_slice(&to_bytes(resp_cases_get.into_body(), 16384).await.unwrap())
            .unwrap();
    let cases_arr = cases_list.as_array().unwrap();
    assert_eq!(cases_arr.len(), 1);
    assert_eq!(cases_arr[0]["id"], "case_incident_42");
    assert_eq!(cases_arr[0]["name"], "Perimeter Breach");
    assert_eq!(cases_arr[0]["event_ids"], json!(["ev_101"]));

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn debug_lpr_simulate_creates_searchable_plate_event_and_snapshots() {
    let tmp_dir = std::env::temp_dir().join(format!("ninjanvr-lpr-sim-{}", rand::random::<u32>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("config.yml");
    let db_path = tmp_dir.join("ninjanvr.db");
    std::fs::write(
            &cfg_path,
            format!(
                "version: 0.18-0\ndatabase:\n  path: {}\nlpr:\n  enabled: true\n  known_plates:\n    BRA2E19: \"Carro do Erik\"\ncameras:\n  portao:\n    enabled: true\n",
                db_path.display()
            ),
        )
        .unwrap();

    let media_dir = tmp_dir.join("media");
    let state = new_app_state(&cfg_path, tmp_dir.join("web"), media_dir.clone(), false)
        .await
        .unwrap();
    let app = build_router(state.clone());

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/debug/lpr-simulate")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"camera":"portao","plate":"bra-2e19","label":"car","score":0.95}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sim_json: Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), 32768).await.unwrap()).unwrap();
    assert_eq!(sim_json["success"], true);
    assert_eq!(sim_json["camera"], "portao");
    assert_eq!(sim_json["plate"], "BRA2E19");
    assert_eq!(sim_json["sub_label"], "Carro do Erik");
    assert_eq!(sim_json["votes"], 3);

    let ev_id = sim_json["event_id"].as_str().unwrap();
    let rev_id = sim_json["review_id"].as_str().unwrap();

    // Verify snapshots and review thumbnails were written to disk
    let clips_dir = media_dir.join("clips");
    assert!(clips_dir.join(format!("{ev_id}.jpg")).exists());
    assert!(clips_dir.join(format!("portao-{ev_id}.webp")).exists());
    assert!(
        clips_dir
            .join(format!("portao-{ev_id}-clean.webp"))
            .exists()
    );
    assert!(
        clips_dir
            .join("review")
            .join(format!("thumb-portao-{rev_id}.webp"))
            .exists()
    );

    // Verify event is searchable by recognized_license_plate via GET /api/events
    let search_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events?recognized_license_plate=BRA2E19")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(search_resp.status(), StatusCode::OK);
    let events_json: Value =
        serde_json::from_slice(&to_bytes(search_resp.into_body(), 65536).await.unwrap()).unwrap();
    let events_arr = events_json.as_array().unwrap();
    assert_eq!(events_arr.len(), 1);
    assert_eq!(events_arr[0]["id"], ev_id);
    assert_eq!(events_arr[0]["sub_label"], "Carro do Erik");
    assert_eq!(events_arr[0]["data"]["recognized_license_plate"], "BRA2E19");

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn genai_parsers_scrub_secrets_skip_gemini_thoughts_filter_models_and_parse_annotations() {
    // 1. scrub_genai_secret_in_error masks active key without corrupting masked/empty keys
    assert_eq!(
        scrub_genai_secret_in_error(
            "HTTP 401 from https://api.example.invalid?key=sk-secret-999".to_string(),
            "sk-secret-999"
        ),
        "HTTP 401 from https://api.example.invalid?key=***"
    );
    assert_eq!(
        scrub_genai_secret_in_error("normal error".to_string(), "***"),
        "normal error"
    );

    // 2. build_effective_genai_config honors overrides, ignores "***", and clears key on clear_api_key
    let saved = ninjanvr_config::GenAiConfig {
        provider: "gemini".to_string(),
        base_url: Some("https://saved.example.invalid".to_string()),
        api_key: Some("sk-saved".to_string()),
        api_key_env: "SAVED_ENV".to_string(),
        ..Default::default()
    };
    let masked_req = GenAiValidateRequest {
        provider: Some(" openai ".to_string()),
        base_url: Some("   ".to_string()),
        api_key: Some("***".to_string()),
        api_key_env: Some("OPENAI_API_KEY".to_string()),
        clear_api_key: Some(false),
        validate: None,
    };
    let eff_masked = build_effective_genai_config(&saved, &masked_req);
    assert_eq!(eff_masked.provider, "openai");
    assert_eq!(eff_masked.base_url, None);
    assert_eq!(eff_masked.api_key.as_deref(), Some("sk-saved"));
    assert_eq!(eff_masked.api_key_env, "OPENAI_API_KEY");

    let clear_req = GenAiValidateRequest {
        provider: None,
        base_url: None,
        api_key: Some("sk-ignored".to_string()),
        api_key_env: None,
        clear_api_key: Some(true),
        validate: None,
    };
    let eff_cleared = build_effective_genai_config(&saved, &clear_req);
    assert_eq!(eff_cleared.api_key, None);

    // 3. extract_models_from_provider_payload filters non-vision/embedding models for Gemini & OpenAI
    let gemini_payload = json!({
        "models": [
            {
                "name": "models/gemini-2.5-flash",
                "supportedGenerationMethods": ["generateContent", "countTokens"]
            },
            {
                "name": "models/text-embedding-004",
                "supportedGenerationMethods": ["embedContent"]
            },
            {
                "name": "models/imagen-3.0-generate-002",
                "supportedGenerationMethods": ["generateContent"]
            },
            {
                "name": "models/veo-2.0-generate-001",
                "supportedGenerationMethods": ["generateContent"]
            }
        ]
    });
    assert_eq!(
        extract_models_from_provider_payload("gemini", &gemini_payload),
        vec!["gemini-2.5-flash".to_string()]
    );

    let openai_payload = json!({
        "data": [
            { "id": "gpt-4o-mini" },
            { "id": "whisper-1" },
            { "id": "tts-1-hd" },
            { "id": "dall-e-3" },
            { "id": "text-embedding-3-small" }
        ]
    });
    assert_eq!(
        extract_models_from_provider_payload("openai", &openai_payload),
        vec!["gpt-4o-mini".to_string()]
    );

    // 4. extract_gemini_candidate_text & parse_ai_judge_json_verdict skip "thought": true parts
    let gemini_thinking_resp = json!({
        "candidates": [{
            "content": {
                "parts": [
                    {
                        "thought": true,
                        "text": "Let me analyze the shadow near the porch..."
                    },
                    {
                        "text": "```json\n{\"is_true_positive\": false, \"confidence\": 0.94, \"reason\": \"Moving tree branch shadow\"}\n```"
                    },
                    {
                        "text": "   ",
                        "thoughtSignature": "trailing_empty_signature_part"
                    }
                ]
            }
        }]
    });
    let (is_tp, conf, reason) = parse_ai_judge_json_verdict(&gemini_thinking_resp).unwrap();
    assert!(!is_tp);
    assert!((conf - 0.94).abs() < 1e-6);
    assert_eq!(reason, "Moving tree branch shadow");

    // 5. parse_ai_auto_annotations converts Gemini box_2d [ymin, xmin, ymax, xmax] (0..1000)
    // and pixel-space bbox into normalized YOLO [cx, cy, w, h] while skipping unknown labels
    let annotate_resp = json!({
        "candidates": [{
            "content": {
                "parts": [
                    { "thought": true, "text": "Detecting objects in frame..." },
                    {
                        "text": "```JSON\n[{\"label\": \"person\", \"box_2d\": [100, 200, 500, 400], \"score\": 0.96}, {\"label\": \"License-Plate\", \"bbox\": [640, 360, 768, 432], \"confidence\": 0.89}, {\"label\": \"alien_spaceship\", \"box_2d\": [0, 0, 100, 100]}]\n```"
                    }
                ]
            }
        }]
    });
    let boxes = parse_ai_auto_annotations(&annotate_resp, 1280, 720).unwrap();
    assert_eq!(boxes.len(), 2);
    assert_eq!(boxes[0].class_id, 0);
    assert_eq!(boxes[0].label, "person");
    // box_2d [ymin=100, xmin=200, ymax=500, xmax=400] -> xmin=0.2, ymin=0.1, xmax=0.4, ymax=0.5 -> cx=0.3, cy=0.3, w=0.2, h=0.4
    assert!((boxes[0].cx - 0.3).abs() < 1e-5);
    assert!((boxes[0].cy - 0.3).abs() < 1e-5);
    assert!((boxes[0].w - 0.2).abs() < 1e-5);
    assert!((boxes[0].h - 0.4).abs() < 1e-5);
    assert_eq!(boxes[1].class_id, 7);
    assert_eq!(boxes[1].label, "license_plate");
}

#[tokio::test]
async fn security_helpers_pkce_protected_paths_share_tokens_range_mp4_and_onvif_status_work() {
    // 1. is_protected_path matrix
    for public_path in [
        "/",
        "/login",
        "/logout",
        "/api/login",
        "/api/logout",
        "/api/auth/first_time_login",
        "/api/auth/providers",
        "/api/auth/oidc/login",
        "/api/auth/oidc/callback",
        "/api/ui/switch",
        "/api/ui/version",
        "/share/some_token",
    ] {
        assert!(
            !is_protected_path(public_path),
            "path '{public_path}' must be public"
        );
    }
    for protected_path in [
        "/api",
        "/api/config",
        "/api/events",
        "/vod/front_door/start/1/end/2/master.m3u8",
        "/vod/clip/front_door/start/1/end/2/master.m3u8",
        "/live/mse/api/ws",
        "/ws",
        "/clips/front_door-123.webp",
        "/recordings/2026-11-02/14/front_door/05.10.mp4",
        "/exports/exp_1.mp4",
    ] {
        assert!(
            is_protected_path(protected_path),
            "path '{protected_path}' must be protected"
        );
    }

    // 2. generate_pkce_pair S256 challenge verification + urlencoding_simple + resolve_external_base_url
    let (state_hex, verifier, challenge) = generate_pkce_pair();
    assert_eq!(state_hex.len(), 32);
    let expected_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    assert_eq!(challenge, expected_challenge);
    assert_eq!(
        urlencoding_simple("https://nvr.example.invalid/cb?a=1&b=two words"),
        "https%3A%2F%2Fnvr.example.invalid%2Fcb%3Fa%3D1%26b%3Dtwo%20words"
    );

    let mut hdrs = HeaderMap::new();
    hdrs.insert("x-forwarded-proto", "https, http".parse().unwrap());
    hdrs.insert(
        "x-forwarded-host",
        "nvr.example.invalid:8443, internal".parse().unwrap(),
    );
    let uri: Uri = "/api/auth/oidc/login".parse().unwrap();
    assert_eq!(
        resolve_external_base_url(&hdrs, &uri),
        "https://nvr.example.invalid:8443"
    );

    // 3. sign_share_token & verify_share_token (valid, expired, tampered, malformed)
    let payload = ShareTokenPayload {
        k: "export".to_string(),
        id: "exp_01".to_string(),
        exp: 2_000,
        cam: None,
        jti: None,
    };
    let token = sign_share_token("top_secret_hmac_key", &payload);
    let verified = verify_share_token("top_secret_hmac_key", &token, 1_500).unwrap();
    assert_eq!(verified.id, "exp_01");
    assert_eq!(
        verify_share_token("top_secret_hmac_key", &token, 2_000).unwrap_err(),
        StatusCode::GONE
    );
    assert_eq!(
        verify_share_token("wrong_key", &token, 1_500).unwrap_err(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        verify_share_token("top_secret_hmac_key", "no_dot_separator", 1_500).unwrap_err(),
        StatusCode::FORBIDDEN
    );

    // 4. serve_mp4_bytes_with_range (open-ended range, clamped end, invalid range fallback)
    let sample_mp4 = b"0123456789ABCDEF".to_vec();
    let mut range_hdrs = HeaderMap::new();
    range_hdrs.insert(header::RANGE, "bytes=10-".parse().unwrap());
    let resp_open = serve_mp4_bytes_with_range(sample_mp4.clone(), &range_hdrs);
    assert_eq!(resp_open.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp_open.headers().get(header::CONTENT_RANGE).unwrap(),
        "bytes 10-15/16"
    );
    let body_open = to_bytes(resp_open.into_body(), 64).await.unwrap();
    assert_eq!(&body_open[..], b"ABCDEF");

    range_hdrs.insert(header::RANGE, "bytes=12-9999".parse().unwrap());
    let resp_clamped = serve_mp4_bytes_with_range(sample_mp4.clone(), &range_hdrs);
    assert_eq!(resp_clamped.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp_clamped.headers().get(header::CONTENT_RANGE).unwrap(),
        "bytes 12-15/16"
    );

    range_hdrs.insert(header::RANGE, "bytes=10-4".parse().unwrap());
    let resp_invalid = serve_mp4_bytes_with_range(sample_mp4.clone(), &range_hdrs);
    assert_eq!(resp_invalid.status(), StatusCode::RANGE_NOT_SATISFIABLE);

    // 5. parse_onvif_status_xml, continuous/relative move SOAP builders, and resolve_recording_disk_path
    let status_xml = r#"
        <s:Envelope>
            <s:Body>
                <tptz:GetStatusResponse>
                    <tptz:PTZStatus>
                        <tt:Position>
                            <tt:PanTilt x="-0.45" y="0.80"/>
                            <tt:Zoom x="0.25"/>
                        </tt:Position>
                        <tt:MoveStatus><tt:PanTilt>moving</tt:PanTilt></tt:MoveStatus>
                        <tt:Error>NoError</tt:Error>
                    </tptz:PTZStatus>
                </tptz:GetStatusResponse>
            </s:Body>
        </s:Envelope>
        "#;
    let parsed_status = parse_onvif_status_xml(status_xml);
    assert!((parsed_status.pan - (-0.45)).abs() < 1e-4);
    assert!((parsed_status.tilt - 0.80).abs() < 1e-4);
    assert!((parsed_status.zoom - 0.25).abs() < 1e-4);
    assert_eq!(parsed_status.move_status, "MOVING");
    assert_eq!(parsed_status.error, None);

    let cont_soap = build_onvif_continuous_move_soap("Profile_1", -2.0, 0.5, 0.0, 120, None, None);
    assert!(cont_soap.contains("<tt:PanTilt x=\"-1.0000\" y=\"0.5000\"/>"));
    assert!(cont_soap.contains("<tptz:Timeout>PT60S</tptz:Timeout>"));

    let rel_soap =
        build_onvif_relative_move_soap("Profile_1", 0.2, -0.2, 0.1, Some(0.01), None, None);
    assert!(rel_soap.contains("<tptz:RelativeMove>"));
    assert!(rel_soap.contains("<tt:PanTilt x=\"0.0500\" y=\"0.0500\"/>"));

    assert_eq!(
        resolve_recording_disk_path(
            Path::new("/custom/media"),
            "/media/ninjanvr/recordings/2026-11-02/14/front_door/05.10.mp4"
        ),
        PathBuf::from("/custom/media/recordings/2026-11-02/14/front_door/05.10.mp4")
    );
}

#[tokio::test]
async fn ws_go2rtc_proxy_delivers_instant_gop_burst_and_prewarm_populates_cache() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-mse-prewarm-{}", rand::random::<u32>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("config.yml");
    let db_path = tmp_dir.join("ninjanvr.db");
    std::fs::write(
        &cfg_path,
        format!(
            "version: 0.18-0\ndatabase:\n  path: {}\ncameras:\n  front_door:\n    enabled: true\n",
            db_path.display()
        ),
    )
    .unwrap();

    // Build synthetic fMP4 init segment + IDR keyframe fragment + P-frame fragment
    let make_box = |box_type: &[u8; 4], payload: &[u8]| -> Vec<u8> {
        let total = (8 + payload.len()) as u32;
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(box_type);
        out.extend_from_slice(payload);
        out
    };
    let make_full_box = |box_type: &[u8; 4], version: u8, flags: u32, body: &[u8]| -> Vec<u8> {
        let total = (12 + body.len()) as u32;
        let mut out = Vec::with_capacity(12 + body.len());
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(box_type);
        let vf = ((version as u32) << 24) | (flags & 0x00FF_FFFF);
        out.extend_from_slice(&vf.to_be_bytes());
        out.extend_from_slice(body);
        out
    };
    let mut init_vec = make_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso6dashmp41");
    init_vec.extend_from_slice(&make_box(b"moov", &[0u8; 24]));
    let init_bytes = Bytes::from(init_vec);

    let build_frag = |is_sync: bool, tag: u8| -> Bytes {
        let flags: u32 = if is_sync { 0x0200_0000 } else { 0x0101_0000 };
        let mfhd = make_full_box(b"mfhd", 0, 0, &1u32.to_be_bytes());
        let tfhd = make_full_box(b"tfhd", 0, 0x020000, &1u32.to_be_bytes());
        let mut trun_body = Vec::new();
        trun_body.extend_from_slice(&1u32.to_be_bytes());
        trun_body.extend_from_slice(&64i32.to_be_bytes());
        trun_body.extend_from_slice(&3000u32.to_be_bytes());
        trun_body.extend_from_slice(&16u32.to_be_bytes());
        trun_body.extend_from_slice(&flags.to_be_bytes());
        let trun = make_full_box(b"trun", 0, 0x000701, &trun_body);
        let mut traf_c = Vec::new();
        traf_c.extend_from_slice(&tfhd);
        traf_c.extend_from_slice(&trun);
        let traf = make_box(b"traf", &traf_c);
        let mut moof_c = Vec::new();
        moof_c.extend_from_slice(&mfhd);
        moof_c.extend_from_slice(&traf);
        let mut out = make_box(b"moof", &moof_c);
        out.extend_from_slice(&make_box(b"mdat", &[tag; 16]));
        Bytes::from(out)
    };
    let keyframe_bytes = build_frag(true, 0xAA);
    let delta_bytes = build_frag(false, 0xBB);
    let codec_str = r#"{"type":"mse","value":"video/mp4; codecs=\"avc1.640029\""}"#;

    // Spin up mock go2rtc WS server
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    let init_srv = init_bytes.clone();
    let key_srv = keyframe_bytes.clone();
    let delta_srv = delta_bytes.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = mock_listener.accept().await {
            let init_c = init_srv.clone();
            let key_c = key_srv.clone();
            let delta_c = delta_srv.clone();
            tokio::spawn(async move {
                if let Ok(ws) = tokio_tungstenite::accept_async(tcp).await {
                    let (mut sink, mut rx) = ws.split();
                    // Wait for client mse handshake
                    let _ = rx.next().await;
                    let _ = sink.send(TungMsg::Text(codec_str.into())).await;
                    let _ = sink.send(TungMsg::Binary(init_c)).await;
                    let _ = sink.send(TungMsg::Binary(key_c)).await;
                    let _ = sink.send(TungMsg::Binary(delta_c)).await;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            });
        }
    });

    let mut state = new_app_state(&cfg_path, tmp_dir.join("web"), tmp_dir.join("media"), false)
        .await
        .unwrap();
    state.go2rtc_ws_base = format!("ws://{mock_addr}");
    let app = build_router(state.clone());

    // 1. Trigger POST /api/go2rtc/streams/front_door/prewarm
    let prewarm_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/go2rtc/streams/front_door/prewarm")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(prewarm_resp.status(), StatusCode::OK);
    let prewarm_json: Value =
        serde_json::from_slice(&to_bytes(prewarm_resp.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(prewarm_json["success"], true);
    assert_eq!(prewarm_json["stream"], "front_door");
    assert_eq!(prewarm_json["warm"], true);

    // Wait briefly for the trailing delta frame to also land in MseGopCache
    tokio::time::sleep(Duration::from_millis(25)).await;

    // 2. Start NinjanVR HTTP server and connect a client WS to /live/mse/api/ws?src=front_door
    let srv_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let srv_addr = srv_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(srv_listener, app).await;
    });

    let client_url = format!("ws://{srv_addr}/live/mse/api/ws?src=front_door");
    let (mut client_ws, _) = tokio_tungstenite::connect_async(&client_url).await.unwrap();
    client_ws
        .send(TungMsg::Text(
            r#"{"type":"mse","value":"avc1.640029"}"#.into(),
        ))
        .await
        .unwrap();

    // Client must immediately receive codec_msg, init_segment, and IDR keyframe fragment from cache
    let msg1 = tokio::time::timeout(Duration::from_millis(500), client_ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(msg1, TungMsg::Text(codec_str.into()));

    let msg2 = tokio::time::timeout(Duration::from_millis(500), client_ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(msg2, TungMsg::Binary(init_bytes));

    let msg3 = tokio::time::timeout(Duration::from_millis(500), client_ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(msg3, TungMsg::Binary(keyframe_bytes));

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn snapshot_download_falls_back_to_detect_frame_as_dated_attachment() {
    let tmp_dir = std::env::temp_dir().join(format!("ninjanvr-snap-dl-{}", rand::random::<u32>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("config.yml");
    let db_path = tmp_dir.join("ninjanvr.db");
    std::fs::write(
        &cfg_path,
        format!(
            "version: 0.18-0\ndatabase:\n  path: {}\ncameras:\n  front_door:\n    enabled: true\n",
            db_path.display()
        ),
    )
    .unwrap();
    let mut state = new_app_state(&cfg_path, tmp_dir.join("web"), tmp_dir.join("media"), false)
        .await
        .unwrap();
    // Unroutable go2rtc so the main-stream grab fails fast and the detect fallback is used.
    state.go2rtc_http_base = "http://127.0.0.1:9".to_string();
    let app = build_router(state.clone());

    // No frame yet: never hand the operator a placeholder image as a "snapshot".
    let empty = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/snapshot.jpg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(empty.status(), StatusCode::SERVICE_UNAVAILABLE);

    let unknown = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/garage/snapshot.jpg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    let (width, height) = (1280u32, 720u32);
    state.latest_yuv_frames.write().await.insert(
        "front_door".to_string(),
        ninjanvr_media::YuvFrame {
            camera: Arc::from("front_door"),
            timestamp: 1_793_700_000.0,
            width,
            height,
            data: Arc::new(vec![128u8; ((width * height * 3) / 2) as usize]),
        },
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/snapshot.jpg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let hdrs = resp.headers().clone();
    assert_eq!(hdrs.get(header::CONTENT_TYPE).unwrap(), "image/jpeg");
    let disposition = hdrs
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(disposition.starts_with("attachment; filename=\"front_door_"));
    assert!(disposition.ends_with(".jpg\""));
    assert_eq!(hdrs.get("x-snapshot-source").unwrap(), "detect");
    assert_eq!(hdrs.get("x-snapshot-resolution").unwrap(), "1280x720");
    assert_eq!(hdrs.get(header::CACHE_CONTROL).unwrap(), "no-store");
    let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
    let img = image::load_from_memory(&bytes).unwrap();
    assert_eq!((img.width(), img.height()), (1280, 720));

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn latest_frame_endpoint_encodes_lazy_yuv_at_native_and_custom_heights() {
    let tmp_dir = std::env::temp_dir().join(format!("ninjanvr-lazy-yuv-{}", rand::random::<u32>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let cfg_path = tmp_dir.join("config.yml");
    let db_path = tmp_dir.join("ninjanvr.db");
    std::fs::write(
        &cfg_path,
        format!(
            "version: 0.18-0\ndatabase:\n  path: {}\ncameras:\n  front_door:\n    enabled: true\n",
            db_path.display()
        ),
    )
    .unwrap();

    let state = new_app_state(&cfg_path, tmp_dir.join("web"), tmp_dir.join("media"), false)
        .await
        .unwrap();

    let width = 1280u32;
    let height = 720u32;
    let yuv_len = ((width * height * 3) / 2) as usize;
    let yuv_frame = ninjanvr_media::YuvFrame {
        camera: Arc::from("front_door"),
        timestamp: 1_793_700_000.0,
        width,
        height,
        data: Arc::new(vec![128u8; yuv_len]),
    };
    state
        .latest_yuv_frames
        .write()
        .await
        .insert("front_door".to_string(), yuv_frame);

    let app = build_router(state);

    // 1. Native resolution (no ?height param) -> 1280x720 JPEG
    let resp_native = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/latest.jpg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_native.status(), StatusCode::OK);
    assert_eq!(
        resp_native.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/jpeg"
    );
    let native_bytes = to_bytes(resp_native.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let native_img = image::load_from_memory(&native_bytes).unwrap();
    assert_eq!(native_img.width(), 1280);
    assert_eq!(native_img.height(), 720);

    // 2. Scaled resolution (?height=360) -> 640x360 JPEG
    let resp_360 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/latest.jpg?height=360")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_360.status(), StatusCode::OK);
    let bytes_360 = to_bytes(resp_360.into_body(), 1024 * 1024).await.unwrap();
    let img_360 = image::load_from_memory(&bytes_360).unwrap();
    assert_eq!(img_360.width(), 640);
    assert_eq!(img_360.height(), 360);

    // 3. WebP format (/latest.webp?height=360) -> 640x360 WebP
    let resp_webp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/latest.webp?height=360")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_webp.status(), StatusCode::OK);
    assert_eq!(
        resp_webp.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/webp"
    );
    let bytes_webp = to_bytes(resp_webp.into_body(), 1024 * 1024).await.unwrap();
    let img_webp = image::load_from_memory(&bytes_webp).unwrap();
    assert_eq!(img_webp.width(), 640);
    assert_eq!(img_webp.height(), 360);

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn share_link_security_hardening_all_eight_findings_covered() {
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr-share-sec-hardening-{}",
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&tmp_dir).unwrap();

    // =========================================================================
    // Finding 1: Secret entropy stretching, .jwt_secret 0o600 permissions,
    // and HMAC-SHA256 domain-separated share signing subkey
    // =========================================================================
    let stretched_short = normalize_jwt_secret_entropy("weak-short-key");
    assert_eq!(stretched_short.len(), 64);
    assert_eq!(
        stretched_short,
        normalize_jwt_secret_entropy("weak-short-key"),
        "KDF stretching must be deterministic"
    );
    assert_ne!(
        stretched_short,
        normalize_jwt_secret_entropy("other-short-key")
    );
    let long_secret = "0123456789abcdef0123456789abcdef_already_32_plus";
    assert_eq!(normalize_jwt_secret_entropy(long_secret), long_secret);

    let jwt_dir = tmp_dir.join("jwt_perm_test");
    std::fs::create_dir_all(&jwt_dir).unwrap();
    let created_secret = load_or_create_jwt_secret(&jwt_dir);
    assert_eq!(created_secret.len(), 128);
    let secret_file = jwt_dir.join(".jwt_secret");
    assert!(secret_file.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&secret_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, ".jwt_secret must be created with mode 0600");
    }
    // Verify empty/whitespace .jwt_secret on disk is regenerated and hardened to 0o600
    let empty_jwt_dir = tmp_dir.join("jwt_empty_test");
    std::fs::create_dir_all(&empty_jwt_dir).unwrap();
    let empty_secret_file = empty_jwt_dir.join(".jwt_secret");
    std::fs::write(&empty_secret_file, "   \n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&empty_secret_file, std::fs::Permissions::from_mode(0o644))
            .unwrap();
    }
    let regenerated_secret = load_or_create_jwt_secret(&empty_jwt_dir);
    assert_eq!(regenerated_secret.len(), 128);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&empty_secret_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
    // Verify existing short .jwt_secret on disk with 0o644 is stretched on load AND hardened to 0o600
    let short_jwt_dir = tmp_dir.join("jwt_short_test");
    std::fs::create_dir_all(&short_jwt_dir).unwrap();
    let short_secret_file = short_jwt_dir.join(".jwt_secret");
    std::fs::write(&short_secret_file, "short_disk_key\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&short_secret_file, std::fs::Permissions::from_mode(0o644))
            .unwrap();
    }
    assert_eq!(
        load_or_create_jwt_secret(&short_jwt_dir),
        normalize_jwt_secret_entropy("short_disk_key")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&short_secret_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "pre-existing .jwt_secret must be hardened to mode 0600"
        );
    }

    // Domain separation: raw HMAC-SHA256(jwt_secret, payload_b64) without subkey/prefix must fail
    let test_payload = ShareTokenPayload {
        k: "export".to_string(),
        id: "front_door_100_200".to_string(),
        exp: unix_now() + 3600,
        cam: Some("front_door".to_string()),
        jti: Some("shr_domain_sep".to_string()),
    };
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&test_payload).unwrap());
    let mut raw_mac = Hmac::<Sha256>::new_from_slice(created_secret.as_bytes()).expect("HMAC key");
    raw_mac.update(payload_b64.as_bytes());
    let raw_sig_b64 =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw_mac.finalize().into_bytes());
    let forged_unprefixed_token = format!("{payload_b64}.{raw_sig_b64}");
    assert_eq!(
        verify_share_token(&created_secret, &forged_unprefixed_token, unix_now()).unwrap_err(),
        StatusCode::FORBIDDEN
    );

    // =========================================================================
    // Setup two-camera test app with auth enforced (front_door + backyard)
    // =========================================================================
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
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
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let auth_app = build_router_with_auth(state.clone(), true);

    let now = unix_now();
    let make_jwt = |sub: &str, role: &str, allowed_cameras: Option<&str>| -> String {
        let claims = JwtClaims {
            sub: sub.to_string(),
            role: role.to_string(),
            allowed_cameras: allowed_cameras.map(|s| s.to_string()),
            iat: now,
            exp: now + 3600,
        };
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };

    state
        .db
        .create_user_with_cameras("scoped_op", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("viewer_u", "pass1234", "viewer", "*", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("temp_op", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    let (_, ro_api_token) = state
        .db
        .create_api_token("ro_sensor", "read_only", None)
        .unwrap();

    let admin_hdr = format!("Bearer {}", make_jwt("admin", "admin", Some("*")));
    let scoped_op_hdr = format!(
        "Bearer {}",
        make_jwt("scoped_op", "operator", Some("front_door"))
    );
    let viewer_hdr = format!("Bearer {}", make_jwt("viewer_u", "viewer", Some("*")));
    let ro_tok_hdr = format!("Bearer {ro_api_token}");
    let temp_op_hdr = format!(
        "Bearer {}",
        make_jwt("temp_op", "operator", Some("front_door"))
    );

    // Seed exports, events, reviews, recordings, and static media files on disk
    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    let clips_previews_backyard = clips_dir.join("previews/backyard");
    let clips_review_dir = clips_dir.join("review");
    let rec_fd_dir = media_dir.join("recordings/2026-11-03/12/front_door");
    let rec_by_dir = media_dir.join("recordings/2026-11-03/12/backyard");
    let rec_staging_by = media_dir.join("recordings/.staging/backyard");
    for d in [
        &exports_dir,
        &clips_dir,
        &clips_previews_backyard,
        &clips_review_dir,
        &rec_fd_dir,
        &rec_by_dir,
        &rec_staging_by,
    ] {
        std::fs::create_dir_all(d).unwrap();
    }

    let mp4_payload: Vec<u8> = (0u8..64u8).collect();
    std::fs::write(exports_dir.join("front_door_100_200.mp4"), &mp4_payload).unwrap();
    std::fs::write(exports_dir.join("backyard_100_200.mp4"), &mp4_payload).unwrap();
    std::fs::write(rec_fd_dir.join("00.00.mp4"), &mp4_payload).unwrap();
    std::fs::write(rec_by_dir.join("00.00.mp4"), &mp4_payload).unwrap();
    std::fs::write(rec_staging_by.join("seg.mp4"), &mp4_payload).unwrap();
    std::fs::write(clips_previews_backyard.join("thumb.webp"), b"WEBP").unwrap();
    std::fs::write(
        clips_review_dir.join("thumb-backyard-100-rev1.webp"),
        b"WEBP",
    )
    .unwrap();
    std::fs::write(clips_dir.join("exp_backyard.webp"), b"WEBP").unwrap();
    std::fs::write(clips_dir.join("front_door-ev_fd_1.webp"), b"WEBP").unwrap();
    std::fs::write(clips_dir.join("mystery_unscoped.webp"), b"WEBP").unwrap();

    state
        .db
        .insert_export(&ExportRow {
            id: "front_door_100_200".to_string(),
            camera: "front_door".to_string(),
            name: "Front Door Export".to_string(),
            date: 1700000100.0,
            video_path: "/media/ninjanvr/exports/front_door_100_200.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/front_door_100_200.webp".to_string(),
            in_progress: false,
        })
        .unwrap();
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_backyard".to_string(),
            camera: "backyard".to_string(),
            name: "Backyard Export".to_string(),
            date: 1700000110.0,
            video_path: "/media/ninjanvr/exports/backyard_100_200.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_backyard.webp".to_string(),
            in_progress: false,
        })
        .unwrap();

    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_fd_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/00.00.mp4".to_string(),
            start_time: 1000.0,
            end_time: 1010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_by_1".to_string(),
            camera: "backyard".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-03/12/backyard/00.00.mp4".to_string(),
            start_time: 1000.0,
            end_time: 1010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

    for (ev_id, cam) in [("ev_fd_1", "front_door"), ("ev_by_1", "backyard")] {
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: ev_id.to_string(),
                camera: cam.to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 1000.0,
                end_time: Some(1008.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.9),
                false_positive: Some(false),
                box_coords: Some(json!([0.1, 0.1, 0.4, 0.8])),
                data: json!({}),
                thumbnail: None,
                area: Some(5000),
                ratio: Some(0.5),
                score: Some(0.9),
                stationary: false,
            })
            .unwrap();
    }
    for (rev_id, cam) in [("rev_fd_1", "front_door"), ("rev_by_1", "backyard")] {
        state
            .db
            .insert_review_segment(&ninjanvr_db::ReviewItemDto {
                id: rev_id.to_string(),
                camera: cam.to_string(),
                start_time: 1000.0,
                end_time: Some(1008.0),
                severity: "alert".to_string(),
                thumb_path: format!("/media/ninjanvr/clips/review/thumb-{cam}-{rev_id}.webp"),
                data: json!({}),
                has_been_reviewed: false,
            })
            .unwrap();
    }

    // =========================================================================
    // Finding 2: Share Token Redaction in Logs & /api/logs/* Admin Restriction
    // =========================================================================
    assert_eq!(sanitize_logged_path("/share/"), "/share/");
    assert_eq!(
        sanitize_logged_path("/share/secret_tok_123"),
        "/share/<redacted>"
    );
    assert_eq!(
        sanitize_logged_path("/share/secret_tok_123/video.mp4"),
        "/share/<redacted>/video.mp4"
    );
    assert_eq!(sanitize_logged_path("/api/version"), "/api/version");
    assert_eq!(sanitize_logged_path("/api/stats"), "/api/stats");

    let mut log_rx = state.log_hub.subscribe();
    state
        .log_hub
        .push_nginx("GET", "/share/raw_secret_token_xyz/video.mp4", 200);
    let (svc, streamed_line) = log_rx.try_recv().unwrap();
    assert_eq!(svc, "nginx");
    assert!(streamed_line.contains("/share/<redacted>/video.mp4"));
    assert!(!streamed_line.contains("raw_secret_token_xyz"));

    // Verify /api/logs/nginx is 403 Forbidden for viewer, read_only token, and operator, 200 OK for admin
    for non_admin_hdr in [&viewer_hdr, &ro_tok_hdr, &scoped_op_hdr] {
        let resp_log_denied = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/logs/nginx")
                    .header(header::AUTHORIZATION, non_admin_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_log_denied.status(), StatusCode::FORBIDDEN);
    }
    let resp_log_admin = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/logs/nginx")
                .header(header::AUTHORIZATION, &admin_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_log_admin.status(), StatusCode::OK);

    // =========================================================================
    // Finding 3: Privacy Masks on Shared Event/Review Clips
    // =========================================================================
    assert!(!has_active_privacy_mask(None));
    assert!(!has_active_privacy_mask(Some(&json!([]))));
    assert!(!has_active_privacy_mask(Some(&json!({}))));
    assert!(!has_active_privacy_mask(Some(&json!(""))));
    assert!(!has_active_privacy_mask(Some(&json!("   "))));
    assert!(has_active_privacy_mask(Some(&json!([[
        [0.1, 0.1],
        [0.2, 0.2],
        [0.1, 0.2]
    ]]))));
    assert!(has_active_privacy_mask(Some(&json!({
        "coordinates": "0.1,0.1,0.5,0.5"
    }))));
    assert!(has_active_privacy_mask(Some(&json!("0.1,0.1,0.5,0.5"))));

    // Camera without privacy_mask streams the raw recording segment
    state
        .db
        .insert_share_link(
            "shr_ev_fd_1",
            "event",
            "ev_fd_1",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let ev_fd_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "event".to_string(),
            id: "ev_fd_1".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_ev_fd_1".to_string()),
        },
    );
    let resp_unmasked_vid = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ev_fd_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_unmasked_vid.status(), StatusCode::OK);
    let unmasked_bytes = to_bytes(resp_unmasked_vid.into_body(), 4096).await.unwrap();
    assert_eq!(&unmasked_bytes[..], &mp4_payload[..]);

    // Enable array privacy_mask on backyard:
    // Case A: when build_mp4_export_with_privacy fails (no segment on disk for ev_by_missing_seg), it fails closed with 404
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("backyard").unwrap().privacy_mask =
            json!([[[0.1, 0.1], [0.5, 0.1], [0.5, 0.5], [0.1, 0.5]]]);
    }
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_by_missing_seg".to_string(),
            camera: "backyard".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 5000.0,
            end_time: Some(5008.0),
            has_clip: true,
            has_snapshot: true,
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
    state
        .db
        .insert_share_link(
            "shr_ev_by_missing",
            "event",
            "ev_by_missing_seg",
            Some("backyard"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let ev_by_missing_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "event".to_string(),
            id: "ev_by_missing_seg".to_string(),
            exp: now + 3600,
            cam: Some("backyard".to_string()),
            jti: Some("shr_ev_by_missing".to_string()),
        },
    );
    let resp_masked_fail_closed = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ev_by_missing_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_masked_fail_closed.status(), StatusCode::NOT_FOUND);

    // Case B: when segment exists on disk (00.00.mp4) but ffmpeg cannot transcode/redact it,
    // build_mp4_export_with_privacy fails closed (404 Not Found), never copies the unredacted raw file,
    // and cleans up any transient DB export row
    state
        .db
        .insert_share_link(
            "shr_ev_by_1",
            "event",
            "ev_by_1",
            Some("backyard"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let ev_by_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "event".to_string(),
            id: "ev_by_1".to_string(),
            exp: now + 3600,
            cam: Some("backyard".to_string()),
            jti: Some("shr_ev_by_1".to_string()),
        },
    );
    let resp_masked_unredactable = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ev_by_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_masked_unredactable.status(), StatusCode::NOT_FOUND);
    assert!(!exports_dir.join("share_redacted_ev_by_1.mp4").exists());
    assert!(
        state
            .db
            .get_export_by_id("share_redacted_ev_by_1")
            .unwrap()
            .is_none()
    );

    // Case C: Pre-generated redacted export (exports/share_redacted_ev_by_1.mp4) is served when privacy_mask is active
    // (string format too) and matching .mask_hash sidecar exists instead of serving the raw unredacted segment from recordings/
    let mask_str_val = json!("0.1,0.1,0.5,0.1,0.5,0.5");
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("backyard").unwrap().privacy_mask = mask_str_val.clone();
    }
    let redacted_bytes = b"REDACTED_MP4_STREAM_PAYLOAD".to_vec();
    std::fs::write(
        exports_dir.join("share_redacted_ev_by_1.mp4"),
        &redacted_bytes,
    )
    .unwrap();
    std::fs::write(
        exports_dir.join(".share_redacted_ev_by_1.mask_hash"),
        compute_privacy_mask_hash(Some(&mask_str_val)),
    )
    .unwrap();
    let resp_masked_redacted = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ev_by_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_masked_redacted.status(), StatusCode::OK);
    let served_redacted = to_bytes(resp_masked_redacted.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(&served_redacted[..], &redacted_bytes[..]);
    assert_ne!(&served_redacted[..], &mp4_payload[..]);

    // =========================================================================
    // Finding 4: Target existence, camera authorization, deleted-user fail-closed,
    // and cross-table camera mismatch protection
    // =========================================================================
    // 4a. Non-existent target returns 404 Not Found
    for kind in ["export", "event", "review"] {
        let resp_404 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "kind": kind,
                            "target_id": "non_existent_id_999"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_404.status(), StatusCode::NOT_FOUND);
    }

    // 4b. Camera-restricted operator ("front_door") gets 403 Forbidden sharing "backyard" export/event/review
    for (kind, target_id) in [
        ("export", "exp_backyard"),
        ("event", "ev_by_1"),
        ("review", "rev_by_1"),
    ] {
        let resp_403 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &scoped_op_hdr)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "kind": kind,
                            "target_id": target_id
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp_403.status(),
            StatusCode::FORBIDDEN,
            "scoped_op must be forbidden from sharing {kind}:{target_id}"
        );
    }

    // 4c. Deleting local operator `temp_op` from SQLite while JWT is still unexpired fails closed (403)
    assert!(state.db.delete_user("temp_op").unwrap());
    state.auth_gate.invalidate_user("temp_op");
    let resp_deleted_share = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &temp_op_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"front_door_100_200"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_deleted_share.status(), StatusCode::FORBIDDEN);

    let resp_deleted_cam_route = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/recordings")
                .header(header::AUTHORIZATION, &temp_op_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_deleted_cam_route.status(), StatusCode::FORBIDDEN);

    // Direct unit coverage of extract_jwt_allowed_cameras & resolve_user_allowed_cameras:
    // - Cookie token extraction as well as Authorization Bearer header & nnvr_ prefix
    let cookie_jwt = make_jwt("oidc_cookie_op", "operator", Some("front_door"));
    let jar_with_cookie = CookieJar::new().add(Cookie::new("ninjanvr_token", cookie_jwt.clone()));
    let empty_hdrs = HeaderMap::new();
    assert_eq!(
        extract_jwt_allowed_cameras(&state, &jar_with_cookie, &empty_hdrs, "ninjanvr_token"),
        Some("front_door".to_string())
    );
    let mut nnvr_hdrs = HeaderMap::new();
    nnvr_hdrs.insert(
        header::AUTHORIZATION,
        "Bearer nnvr_api_tok".parse().unwrap(),
    );
    assert_eq!(
        extract_jwt_allowed_cameras(&state, &CookieJar::new(), &nnvr_hdrs, "ninjanvr_token"),
        None
    );

    // - token: prefix user in resolve_user_allowed_cameras returns None
    let base_auth_cfg = state.config.read().await.auth.clone();
    assert_eq!(
        resolve_user_allowed_cameras(
            &state,
            &CookieJar::new(),
            &empty_hdrs,
            &base_auth_cfg,
            "token:ro_sensor",
            "read_only"
        ),
        None
    );

    // - db_u.role == "admin" in SQLite returns None even if passed role is "operator"
    state
        .db
        .create_user_with_cameras("db_admin_u", "pass1234", "admin", "front_door", 1000)
        .unwrap();
    assert_eq!(
        resolve_user_allowed_cameras(
            &state,
            &CookieJar::new(),
            &empty_hdrs,
            &base_auth_cfg,
            "db_admin_u",
            "operator"
        ),
        None
    );
    state.db.delete_user("db_admin_u").unwrap();
    state.auth_gate.invalidate_user("db_admin_u");

    // - OIDC enabled + non-empty issuer_url for OIDC JWT user not in local SQLite:
    //   Some("front_door"), Some("*"), and None
    let mut oidc_auth_cfg = base_auth_cfg.clone();
    oidc_auth_cfg.oidc.enabled = true;
    oidc_auth_cfg.oidc.issuer_url = "https://idp.example.test".to_string();
    assert_eq!(
        resolve_user_allowed_cameras(
            &state,
            &jar_with_cookie,
            &empty_hdrs,
            &oidc_auth_cfg,
            "oidc_cookie_op",
            "operator"
        ),
        Some(vec!["front_door".to_string()])
    );
    let jar_oidc_star = CookieJar::new().add(Cookie::new(
        "ninjanvr_token",
        make_jwt("oidc_star_op", "operator", Some("*")),
    ));
    assert_eq!(
        resolve_user_allowed_cameras(
            &state,
            &jar_oidc_star,
            &empty_hdrs,
            &oidc_auth_cfg,
            "oidc_star_op",
            "operator"
        ),
        None
    );
    let jar_oidc_none = CookieJar::new().add(Cookie::new(
        "ninjanvr_token",
        make_jwt("oidc_none_op", "operator", None),
    ));
    assert_eq!(
        resolve_user_allowed_cameras(
            &state,
            &jar_oidc_none,
            &empty_hdrs,
            &oidc_auth_cfg,
            "oidc_none_op",
            "operator"
        ),
        Some(Vec::new())
    );

    // - Proxy enabled with Remote-User header returns None
    let mut proxy_auth_cfg = base_auth_cfg.clone();
    proxy_auth_cfg.proxy.enabled = true;
    let mut proxy_hdrs = HeaderMap::new();
    proxy_hdrs.insert("remote-user", "proxy_op".parse().unwrap());
    assert_eq!(
        resolve_user_allowed_cameras(
            &state,
            &CookieJar::new(),
            &proxy_hdrs,
            &proxy_auth_cfg,
            "proxy_op",
            "operator"
        ),
        None
    );

    // - Even when oidc.enabled = true with empty issuer_url, or proxy.enabled = true without proxy headers,
    //   or when all local users are deleted (list_users() is empty), a deleted user's JWT still fails closed
    let mut unconfigured_oidc_cfg = base_auth_cfg.clone();
    unconfigured_oidc_cfg.oidc.enabled = true;
    unconfigured_oidc_cfg.oidc.issuer_url = String::new();
    unconfigured_oidc_cfg.proxy.enabled = true;
    assert_eq!(
        resolve_user_allowed_cameras(
            &state,
            &jar_with_cookie,
            &empty_hdrs,
            &unconfigured_oidc_cfg,
            "oidc_cookie_op",
            "operator"
        ),
        Some(Vec::new())
    );

    // 4d. Cross-table camera mismatch, payload.cam mismatch, and fallback resolution
    state
        .db
        .insert_share_link(
            "shr_mismatch_payload",
            "event",
            "ev_by_1",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let mismatch_payload_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "event".to_string(),
            id: "ev_by_1".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_mismatch_payload".to_string()),
        },
    );
    for uri in [
        format!("/share/{mismatch_payload_tok}"),
        format!("/share/{mismatch_payload_tok}/video.mp4"),
    ] {
        let r = auth_app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "mismatch on {uri}");
    }

    // Colliding ID across event ("front_door") and review ("backyard") tables -> 403 Forbidden
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "collision_id_1".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 1000.0,
            end_time: Some(1008.0),
            has_clip: true,
            has_snapshot: true,
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
    state
        .db
        .insert_review_segment(&ninjanvr_db::ReviewItemDto {
            id: "collision_id_1".to_string(),
            camera: "backyard".to_string(),
            start_time: 1000.0,
            end_time: Some(1008.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-backyard-collision_id_1.webp"
                .to_string(),
            data: json!({}),
            has_been_reviewed: false,
        })
        .unwrap();
    state
        .db
        .insert_share_link(
            "shr_collision_1",
            "review",
            "collision_id_1",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let collision_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "review".to_string(),
            id: "collision_id_1".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_collision_1".to_string()),
        },
    );
    for uri in [
        format!("/share/{collision_tok}"),
        format!("/share/{collision_tok}/video.mp4"),
    ] {
        let r = auth_app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    // Direct and router coverage of resolve_share_target_metadata, share_video_mp4, and share_player_page:
    // - kind == "review" falling back to get_event when get_review_by_id is None
    let (fb_ev_cam, fb_ev_title, _, fb_ev_exists) =
        resolve_share_target_metadata(&state, "review", "ev_fd_1");
    assert!(fb_ev_exists);
    assert_eq!(fb_ev_cam.as_deref(), Some("front_door"));
    assert!(fb_ev_title.contains("front_door"));

    // - kind == "event" falling back to get_review_by_id when get_event is None
    let (fb_rev_cam, fb_rev_title, _, fb_rev_exists) =
        resolve_share_target_metadata(&state, "event", "rev_fd_1");
    assert!(fb_rev_exists);
    assert_eq!(fb_rev_cam.as_deref(), Some("front_door"));
    assert!(fb_rev_title.contains("front_door"));

    // - kind == "export" disk-fallback when get_export_by_id is None
    std::fs::write(
        exports_dir.join("front_door_diskonly_300.mp4"),
        &mp4_payload,
    )
    .unwrap();
    let (fb_disk_cam, _, _, fb_disk_exists) =
        resolve_share_target_metadata(&state, "export", "front_door_diskonly_300");
    assert!(fb_disk_exists);
    assert_eq!(fb_disk_cam.as_deref(), Some("front_door"));

    // - share_video_mp4 for kind == "review" happy path (200 OK) and token_cam mismatch (403 Forbidden)
    state
        .db
        .insert_share_link(
            "shr_rev_fd_ok",
            "review",
            "rev_fd_1",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let rev_fd_ok_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "review".to_string(),
            id: "rev_fd_1".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_rev_fd_ok".to_string()),
        },
    );
    let resp_rev_fd_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{rev_fd_ok_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_fd_ok.status(), StatusCode::OK);

    state
        .db
        .insert_share_link(
            "shr_rev_fd_mismatch",
            "review",
            "rev_fd_1",
            Some("backyard"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let rev_fd_mismatch_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "review".to_string(),
            id: "rev_fd_1".to_string(),
            exp: now + 3600,
            cam: Some("backyard".to_string()),
            jti: Some("shr_rev_fd_mismatch".to_string()),
        },
    );
    let resp_rev_fd_mismatch = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{rev_fd_mismatch_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_fd_mismatch.status(), StatusCode::FORBIDDEN);

    // - share_video_mp4 for kind == "review" falling back to get_event (200 OK and 403 mismatch)
    state
        .db
        .insert_share_link(
            "shr_rev_fb_ev_ok",
            "review",
            "ev_fd_1",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let rev_fb_ev_ok_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "review".to_string(),
            id: "ev_fd_1".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_rev_fb_ev_ok".to_string()),
        },
    );
    let resp_rev_fb_ev_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{rev_fb_ev_ok_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_fb_ev_ok.status(), StatusCode::OK);

    state
        .db
        .insert_share_link(
            "shr_rev_fb_ev_mismatch",
            "review",
            "ev_fd_1",
            Some("backyard"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let rev_fb_ev_mismatch_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "review".to_string(),
            id: "ev_fd_1".to_string(),
            exp: now + 3600,
            cam: Some("backyard".to_string()),
            jti: Some("shr_rev_fb_ev_mismatch".to_string()),
        },
    );
    let resp_rev_fb_ev_mismatch = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{rev_fb_ev_mismatch_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_fb_ev_mismatch.status(), StatusCode::FORBIDDEN);

    // - share_video_mp4 for kind == "event" falling back to get_review_by_id (200 OK and 403 mismatch)
    state
        .db
        .insert_share_link(
            "shr_ev_fb_rev_ok",
            "event",
            "rev_fd_1",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let ev_fb_rev_ok_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "event".to_string(),
            id: "rev_fd_1".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_ev_fb_rev_ok".to_string()),
        },
    );
    let resp_ev_fb_rev_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ev_fb_rev_ok_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ev_fb_rev_ok.status(), StatusCode::OK);

    state
        .db
        .insert_share_link(
            "shr_ev_fb_rev_mismatch",
            "event",
            "rev_fd_1",
            Some("backyard"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let ev_fb_rev_mismatch_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "event".to_string(),
            id: "rev_fd_1".to_string(),
            exp: now + 3600,
            cam: Some("backyard".to_string()),
            jti: Some("shr_ev_fb_rev_mismatch".to_string()),
        },
    );
    let resp_ev_fb_rev_mismatch = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ev_fb_rev_mismatch_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ev_fb_rev_mismatch.status(), StatusCode::FORBIDDEN);

    // - share_video_mp4 for kind == "export" when token_cam != exp_cam (403) and disk-fallback (200 & 403)
    state
        .db
        .insert_share_link(
            "shr_exp_mismatch",
            "export",
            "exp_backyard",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let exp_mismatch_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_backyard".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_exp_mismatch".to_string()),
        },
    );
    let resp_exp_mismatch = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{exp_mismatch_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_exp_mismatch.status(), StatusCode::FORBIDDEN);

    state
        .db
        .insert_share_link(
            "shr_exp_disk_ok",
            "export",
            "front_door_diskonly_300",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let exp_disk_ok_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "front_door_diskonly_300".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_exp_disk_ok".to_string()),
        },
    );
    let resp_exp_disk_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{exp_disk_ok_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_exp_disk_ok.status(), StatusCode::OK);

    state
        .db
        .insert_share_link(
            "shr_exp_disk_mismatch",
            "export",
            "front_door_diskonly_300",
            Some("backyard"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let exp_disk_mismatch_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "front_door_diskonly_300".to_string(),
            exp: now + 3600,
            cam: Some("backyard".to_string()),
            jti: Some("shr_exp_disk_mismatch".to_string()),
        },
    );
    let resp_exp_disk_mismatch = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{exp_disk_mismatch_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_exp_disk_mismatch.status(), StatusCode::FORBIDDEN);

    // - share_player_page returning 404 Not Found when target does not exist / was deleted after token creation
    state
        .db
        .insert_share_link(
            "shr_deleted_target_999",
            "event",
            "deleted_after_share_999",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let deleted_target_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "event".to_string(),
            id: "deleted_after_share_999".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_deleted_target_999".to_string()),
        },
    );
    let resp_player_404 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{deleted_target_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_player_404.status(), StatusCode::NOT_FOUND);
    state.db.prune_expired_share_links(f64::MAX).unwrap();

    // =========================================================================
    // Finding 5: Bounded Async File Streaming (`serve_mp4_file_with_range`)
    // =========================================================================
    assert!(
        serve_mp4_file_with_range(&exports_dir, &empty_hdrs)
            .await
            .is_none()
    );

    let file_disk_path = exports_dir.join("front_door_100_200.mp4");
    let full_resp = serve_mp4_file_with_range(&file_disk_path, &empty_hdrs)
        .await
        .unwrap();
    assert_eq!(full_resp.status(), StatusCode::OK);
    for hdr_name in [
        header::CONTENT_SECURITY_POLICY,
        header::X_CONTENT_TYPE_OPTIONS,
        header::REFERRER_POLICY,
        header::X_FRAME_OPTIONS,
        header::CACHE_CONTROL,
    ] {
        assert!(
            full_resp.headers().contains_key(&hdr_name),
            "missing security header {hdr_name} on 200 OK file response"
        );
    }
    let full_bytes = to_bytes(full_resp.into_body(), 4096).await.unwrap();
    assert_eq!(&full_bytes[..], &mp4_payload[..]);

    let mut open_hdrs = HeaderMap::new();
    open_hdrs.insert(header::RANGE, "bytes=10-".parse().unwrap());
    let open_resp = serve_mp4_file_with_range(&file_disk_path, &open_hdrs)
        .await
        .unwrap();
    assert_eq!(open_resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        open_resp.headers().get(header::CONTENT_RANGE).unwrap(),
        "bytes 10-63/64"
    );
    for hdr_name in [
        header::CONTENT_SECURITY_POLICY,
        header::X_CONTENT_TYPE_OPTIONS,
        header::REFERRER_POLICY,
        header::X_FRAME_OPTIONS,
        header::CACHE_CONTROL,
    ] {
        assert!(
            open_resp.headers().contains_key(&hdr_name),
            "missing security header {hdr_name} on 206 Partial Content file response"
        );
    }
    let open_bytes = to_bytes(open_resp.into_body(), 4096).await.unwrap();
    assert_eq!(&open_bytes[..], &mp4_payload[10..]);

    let mut bounded_hdrs = HeaderMap::new();
    bounded_hdrs.insert(header::RANGE, "bytes=0-3".parse().unwrap());
    let bounded_resp = serve_mp4_file_with_range(&file_disk_path, &bounded_hdrs)
        .await
        .unwrap();
    assert_eq!(bounded_resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        bounded_resp.headers().get(header::CONTENT_RANGE).unwrap(),
        "bytes 0-3/64"
    );
    let bounded_bytes = to_bytes(bounded_resp.into_body(), 4096).await.unwrap();
    assert_eq!(&bounded_bytes[..], &mp4_payload[0..=3]);

    let mut inverted_hdrs = HeaderMap::new();
    inverted_hdrs.insert(header::RANGE, "bytes=10-4".parse().unwrap());
    let inverted_resp = serve_mp4_file_with_range(&file_disk_path, &inverted_hdrs)
        .await
        .unwrap();
    assert_eq!(inverted_resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);

    let mut oob_hdrs = HeaderMap::new();
    oob_hdrs.insert(header::RANGE, "bytes=9999-10000".parse().unwrap());
    let oob_resp = serve_mp4_file_with_range(&file_disk_path, &oob_hdrs)
        .await
        .unwrap();
    assert_eq!(oob_resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);

    // =========================================================================
    // Finding 6 & Finding 8: Share Link Persistence, Listing, Revocation & Security Headers
    // =========================================================================
    let resp_create_fd = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &scoped_op_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"front_door_100_200","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_create_fd.status(), StatusCode::OK);
    let create_fd_json: Value =
        serde_json::from_slice(&to_bytes(resp_create_fd.into_body(), 4096).await.unwrap()).unwrap();
    let fd_share_id = create_fd_json["id"].as_str().unwrap().to_string();
    let fd_share_token = create_fd_json["token"].as_str().unwrap().to_string();

    let resp_create_by = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &admin_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_backyard","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_create_by.status(), StatusCode::OK);
    let create_by_json: Value =
        serde_json::from_slice(&to_bytes(resp_create_by.into_body(), 4096).await.unwrap()).unwrap();
    let by_share_id = create_by_json["id"].as_str().unwrap().to_string();

    // Finding 8: Verify HTML response headers on GET /share/{token}
    let resp_player = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{fd_share_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_player.status(), StatusCode::OK);
    let player_hdrs = resp_player.headers();
    assert!(
        player_hdrs
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("default-src 'none'")
    );
    assert_eq!(
        player_hdrs.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );
    assert_eq!(
        player_hdrs.get(header::REFERRER_POLICY).unwrap(),
        "no-referrer"
    );
    assert_eq!(player_hdrs.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
    assert_eq!(
        player_hdrs.get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );

    // Verify nginx access log redacted the share token from the router request above
    let (_, nginx_lines) = state.log_hub.query_lines("nginx", None, None);
    assert!(
        nginx_lines.iter().any(|l| l.contains("/share/<redacted>")),
        "nginx log buffer must contain redacted share path"
    );
    assert!(
        !nginx_lines.iter().any(|l| l.contains(&fd_share_token)),
        "nginx log buffer must never contain raw share token"
    );

    // Finding 6: GET /api/share filters by caller_allowed_cameras for scoped_op
    let resp_list_scoped = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/share")
                .header(header::AUTHORIZATION, &scoped_op_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_list_scoped.status(), StatusCode::OK);
    let list_scoped_arr: Value =
        serde_json::from_slice(&to_bytes(resp_list_scoped.into_body(), 8192).await.unwrap())
            .unwrap();
    let scoped_links = list_scoped_arr.as_array().unwrap();
    assert_eq!(scoped_links.len(), 1);
    assert_eq!(scoped_links[0]["id"], fd_share_id);

    // DELETE /api/share/{id} returns 403 for unauthorized camera, 404 for unknown ID, 200 OK for permitted camera
    let resp_revoke_forbidden = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/share/{by_share_id}"))
                .header(header::AUTHORIZATION, &scoped_op_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_revoke_forbidden.status(), StatusCode::FORBIDDEN);

    let resp_revoke_404 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/share/shr_unknown_999")
                .header(header::AUTHORIZATION, &admin_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_revoke_404.status(), StatusCode::NOT_FOUND);

    let resp_revoke_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/share/{fd_share_id}"))
                .header(header::AUTHORIZATION, &scoped_op_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_revoke_ok.status(), StatusCode::OK);

    // Subsequent GET /share/{token} and GET /share/{token}/video.mp4 return 410 Gone
    for uri in [
        format!("/share/{fd_share_token}"),
        format!("/share/{fd_share_token}/video.mp4"),
    ] {
        let r = auth_app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::GONE,
            "revoked link must return 410 Gone on {uri}"
        );
    }

    // =========================================================================
    // Finding 7: Per-Camera Scoping on /api/exports, /api/export/*, and Static Media
    // =========================================================================
    // Direct unit coverage of resolve_static_media_camera for all path shapes:
    assert_eq!(
        resolve_static_media_camera(&state, "/exports/share_redacted_ev_fd_1.mp4").as_deref(),
        Some("front_door")
    );
    assert_eq!(
        resolve_static_media_camera(&state, "/exports/share_redacted_rev_by_1.mp4").as_deref(),
        Some("backyard")
    );
    assert_eq!(
        resolve_static_media_camera(&state, "/recordings/front_door/00.00.mp4").as_deref(),
        Some("front_door")
    );
    assert_eq!(resolve_static_media_camera(&state, "/recordings/"), None);
    assert_eq!(
        resolve_static_media_camera(&state, "/clips/review/thumb-front_door-rev1.webp").as_deref(),
        Some("front_door")
    );
    assert_eq!(
        resolve_static_media_camera(&state, "/clips/review/not_a_thumb.webp"),
        None
    );
    assert_eq!(
        resolve_static_media_camera(&state, "/clips/ev_fd_1.webp").as_deref(),
        Some("front_door")
    );
    assert_eq!(
        resolve_static_media_camera(&state, "/clips/rev_by_1.webp").as_deref(),
        Some("backyard")
    );
    assert_eq!(
        resolve_static_media_camera(&state, "/clips/share_redacted_ev_fd_1.webp").as_deref(),
        Some("front_door")
    );

    let resp_exps_scoped = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/exports")
                .header(header::AUTHORIZATION, &scoped_op_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_exps_scoped.status(), StatusCode::OK);
    let exps_scoped_arr: Value =
        serde_json::from_slice(&to_bytes(resp_exps_scoped.into_body(), 8192).await.unwrap())
            .unwrap();
    let exps_scoped = exps_scoped_arr.as_array().unwrap();
    assert_eq!(exps_scoped.len(), 1);
    assert_eq!(exps_scoped[0]["id"], "front_door_100_200");

    let resp_rename_by = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/export/exp_backyard/rename")
                .header(header::AUTHORIZATION, &scoped_op_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Hijacked"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rename_by.status(), StatusCode::FORBIDDEN);

    let resp_rename_fd_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/export/front_door_100_200/rename")
                .header(header::AUTHORIZATION, &scoped_op_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Front Door Renamed"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rename_fd_ok.status(), StatusCode::OK);

    let resp_bulk_del_by = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/exports/delete")
                .header(header::AUTHORIZATION, &scoped_op_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ids":["exp_backyard"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_bulk_del_by.status(), StatusCode::FORBIDDEN);

    // Seed a second front_door export to verify scoped operator bulk delete & admin single delete
    state
        .db
        .insert_export(&ExportRow {
            id: "front_door_300_400".to_string(),
            camera: "front_door".to_string(),
            name: "Front Door Export 2".to_string(),
            date: 1700000200.0,
            video_path: "/media/ninjanvr/exports/front_door_300_400.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/front_door_300_400.webp".to_string(),
            in_progress: false,
        })
        .unwrap();
    let resp_bulk_del_fd_op = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/exports/delete")
                .header(header::AUTHORIZATION, &scoped_op_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ids":["front_door_300_400"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_bulk_del_fd_op.status(), StatusCode::FORBIDDEN);
    let resp_bulk_del_fd_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/exports/delete")
                .header(header::AUTHORIZATION, &admin_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ids":["front_door_300_400"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_bulk_del_fd_ok.status(), StatusCode::OK);
    assert!(
        state
            .db
            .get_export_by_id("front_door_300_400")
            .unwrap()
            .is_none()
    );

    for forbidden_static in [
        "/exports/backyard_100_200.mp4",
        "/exports/share_redacted_ev_by_1.mp4",
        "/recordings/2026-11-03/12/backyard/00.00.mp4",
        "/recordings/.staging/backyard/seg.mp4",
        "/clips/previews/backyard/thumb.webp",
        "/clips/review/thumb-backyard-100-rev1.webp",
        "/clips/exp_backyard.webp",
        "/clips/mystery_unscoped.webp",
    ] {
        let r = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(forbidden_static)
                    .header(header::AUTHORIZATION, &scoped_op_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::FORBIDDEN,
            "scoped_op must get 403 Forbidden on {forbidden_static}"
        );
    }

    for allowed_static in [
        "/exports/front_door_100_200.mp4",
        "/recordings/2026-11-03/12/front_door/00.00.mp4",
        "/clips/front_door-ev_fd_1.webp",
    ] {
        let r = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(allowed_static)
                    .header(header::AUTHORIZATION, &scoped_op_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::OK,
            "scoped_op must get 200 OK on {allowed_static}"
        );
    }

    // Finally, verify admin DELETE /api/export/{id} succeeds and verify deleted local user
    // still fails closed when all local SQLite users are deleted (list_users() is empty)
    let resp_admin_del_exp = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/export/exp_backyard")
                .header(header::AUTHORIZATION, &admin_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_admin_del_exp.status(), StatusCode::OK);

    let mut empty_users_state = state.clone();
    empty_users_state.db = NinjaDatabase::open(tmp_dir.join("empty_users.db")).unwrap();
    assert!(empty_users_state.db.list_users().unwrap().is_empty());
    assert_eq!(
        resolve_user_allowed_cameras(
            &empty_users_state,
            &CookieJar::new(),
            &empty_hdrs,
            &base_auth_cfg,
            "scoped_op",
            "operator"
        ),
        Some(Vec::new()),
        "deleted local user must fail closed even when SQLite user table is empty"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_share_link_phase1_jti_allowlist_kind_validation_and_lifecycle_revocation_work() {
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr_sec_share_phase1_{}",
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    let yaml = format!(
        r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
  sala:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/sala
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let auth_app = build_router_with_auth(state.clone(), true);

    let now = unix_now();
    let make_jwt = |sub: &str, role: &str, allowed_cameras: Option<&str>| -> String {
        let claims = JwtClaims {
            sub: sub.to_string(),
            role: role.to_string(),
            allowed_cameras: allowed_cameras.map(|s| s.to_string()),
            iat: now,
            exp: now + 3600,
        };
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };

    state
        .db
        .create_user_with_cameras(
            "op_lifecycle",
            "pass1234",
            "operator",
            "front_door,backyard",
            1000,
        )
        .unwrap();
    state
        .db
        .create_user_with_cameras("op_to_delete", "pass1234", "operator", "front_door", 1000)
        .unwrap();

    let admin_hdr = format!("Bearer {}", make_jwt("admin", "admin", Some("*")));
    let op_lifecycle_hdr = format!(
        "Bearer {}",
        make_jwt("op_lifecycle", "operator", Some("front_door,backyard"))
    );
    let op_del_hdr = format!(
        "Bearer {}",
        make_jwt("op_to_delete", "operator", Some("front_door"))
    );

    let exports_dir = media_dir.join("exports");
    let rec_fd_dir = media_dir.join("recordings/2026-11-03/12/front_door");
    let rec_sala_dir = media_dir.join("recordings/2026-11-03/12/sala");
    std::fs::create_dir_all(&exports_dir).unwrap();
    std::fs::create_dir_all(&rec_fd_dir).unwrap();
    std::fs::create_dir_all(&rec_sala_dir).unwrap();

    let mp4_payload: Vec<u8> = (0u8..64u8).collect();
    std::fs::write(exports_dir.join("exp_fd_1.mp4"), &mp4_payload).unwrap();
    std::fs::write(exports_dir.join("exp_fd_2.mp4"), &mp4_payload).unwrap();
    std::fs::write(exports_dir.join("share_redacted_ev_secret.mp4"), b"SECRET").unwrap();
    std::fs::write(exports_dir.join("share_clip_ev_secret.mp4"), b"SECRET").unwrap();
    std::fs::write(exports_dir.join(".hidden_export.mp4"), b"SECRET").unwrap();
    std::fs::write(rec_fd_dir.join("00.00.mp4"), &mp4_payload).unwrap();
    std::fs::write(rec_sala_dir.join("00.00.mp4"), b"SALA_FALLBACK_LEAK").unwrap();

    for exp_id in ["exp_fd_1", "exp_fd_2", "exp_fd_keep"] {
        std::fs::write(exports_dir.join(format!("{exp_id}.mp4")), &mp4_payload).unwrap();
        state
            .db
            .insert_export(&ExportRow {
                id: exp_id.to_string(),
                camera: "front_door".to_string(),
                name: format!("Export {exp_id}"),
                date: 1700000100.0,
                video_path: format!("/media/ninjanvr/exports/{exp_id}.mp4"),
                thumb_path: format!("/media/ninjanvr/clips/{exp_id}.webp"),
                in_progress: false,
            })
            .unwrap();
    }

    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_fd_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/00.00.mp4".to_string(),
            start_time: 1000.0,
            end_time: 1010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_sala_1".to_string(),
            camera: "sala".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-03/12/sala/00.00.mp4".to_string(),
            start_time: 1000.0,
            end_time: 1010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

    for ev_id in ["ev_p1_1", "ev_p1_2"] {
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: ev_id.to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 1000.0,
                end_time: Some(1008.0),
                has_clip: true,
                has_snapshot: true,
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
    }

    // Event with empty camera must NOT fall back to "sala"
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_empty_cam".to_string(),
            camera: String::new(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 1000.0,
            end_time: Some(1008.0),
            has_clip: true,
            has_snapshot: true,
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

    for rev_id in ["rev_p1_1", "rev_p1_2"] {
        state
            .db
            .insert_review_segment(&ninjanvr_db::ReviewItemDto {
                id: rev_id.to_string(),
                camera: "front_door".to_string(),
                start_time: 1000.0,
                end_time: Some(1008.0),
                severity: "alert".to_string(),
                thumb_path: format!("/media/ninjanvr/clips/review/thumb-front_door-{rev_id}.webp"),
                data: json!({}),
                has_been_reviewed: false,
            })
            .unwrap();
    }

    // =========================================================================
    // 1. High-entropy 128-bit JTI & Fail-Closed Allowlist Verification (Finding 3)
    // =========================================================================
    let resp_create = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &op_lifecycle_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_fd_keep","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_create.status(), StatusCode::OK);
    let create_json: Value =
        serde_json::from_slice(&to_bytes(resp_create.into_body(), 4096).await.unwrap()).unwrap();
    let jti = create_json["id"].as_str().unwrap().to_string();
    let token = create_json["token"].as_str().unwrap().to_string();
    let parts: Vec<&str> = jti.split('_').collect();
    assert_eq!(parts.len(), 3, "JTI must have shr_<ts>_<32hex> format");
    assert_eq!(parts[0], "shr");
    assert_eq!(
        parts[2].len(),
        32,
        "JTI random suffix must be 32 hex chars (128 bits)"
    );
    assert!(
        parts[2]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );

    // Unregistered JTI (not present in share_link table) -> 410 GONE
    let unregistered_jti_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_fd_keep".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_1700000000_0123456789abcdef0123456789abcdef".to_string()),
        },
    );
    for uri in [
        format!("/share/{unregistered_jti_tok}"),
        format!("/share/{unregistered_jti_tok}/video.mp4"),
    ] {
        let r = auth_app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::GONE,
            "unregistered JTI must return 410 GONE on {uri}"
        );
    }

    // Expired DB row (`expires_at <= now_ts`) even if signed payload.exp is in the future -> 410 GONE
    state
        .db
        .insert_share_link(
            "shr_db_expired_0001",
            "export",
            "exp_fd_keep",
            Some("front_door"),
            "admin",
            (now - 7200) as f64,
            (now - 10) as f64,
        )
        .unwrap();
    let db_expired_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_fd_keep".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_db_expired_0001".to_string()),
        },
    );
    for uri in [
        format!("/share/{db_expired_tok}"),
        format!("/share/{db_expired_tok}/video.mp4"),
    ] {
        let r = auth_app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::GONE,
            "DB-expired JTI must return 410 GONE on {uri}"
        );
    }

    // Payload mismatch against DB record (kind, target_id, or camera) -> 403 FORBIDDEN
    for mismatched_payload in [
        ShareTokenPayload {
            k: "event".to_string(),
            id: "exp_fd_keep".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some(jti.clone()),
        },
        ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_fd_1".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some(jti.clone()),
        },
        ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_fd_keep".to_string(),
            exp: now + 3600,
            cam: Some("backyard".to_string()),
            jti: Some(jti.clone()),
        },
    ] {
        let bad_tok = sign_share_token(&state.jwt_secret, &mismatched_payload);
        for uri in [
            format!("/share/{bad_tok}"),
            format!("/share/{bad_tok}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::FORBIDDEN,
                "DB/payload mismatch must return 403 FORBIDDEN on {uri}"
            );
        }
    }

    // =========================================================================
    // 2. Strict Kind Validation, No 'sala' Fallback, Block Internal Export Prefixes (Finding 7)
    // =========================================================================
    for invalid_kind in ["recording", "clip", "unknown", ""] {
        let invalid_kind_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: invalid_kind.to_string(),
                id: "exp_fd_keep".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: None,
            },
        );
        assert_eq!(
            verify_share_token(&state.jwt_secret, &invalid_kind_tok, now).unwrap_err(),
            StatusCode::FORBIDDEN
        );
        for uri in [
            format!("/share/{invalid_kind_tok}"),
            format!("/share/{invalid_kind_tok}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN);
        }
    }

    // Event with empty camera does NOT fall back to "sala"
    let (_, _, _, empty_cam_exists) =
        resolve_share_target_metadata(&state, "event", "ev_empty_cam");
    assert!(!empty_cam_exists);
    for kind in ["event", "review"] {
        let jti_ec = format!("shr_empty_cam_{kind}");
        state
            .db
            .insert_share_link(
                &jti_ec,
                kind,
                "ev_empty_cam",
                None,
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let empty_cam_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: kind.to_string(),
                id: "ev_empty_cam".to_string(),
                exp: now + 3600,
                cam: None,
                jti: Some(jti_ec),
            },
        );
        for uri in [
            format!("/share/{empty_cam_tok}"),
            format!("/share/{empty_cam_tok}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::NOT_FOUND,
                "empty-camera event must fail closed with 404 on {uri}"
            );
        }
    }

    // Internal prefixes ("share_redacted_", "share_clip_", ".") blocked in export disk-fallback
    for blocked_id in [
        "share_redacted_ev_secret",
        "share_clip_ev_secret",
        ".hidden_export",
    ] {
        let (_, _, _, exists) = resolve_share_target_metadata(&state, "export", blocked_id);
        assert!(
            !exists,
            "internal/hidden export disk artifact {blocked_id} must be rejected"
        );
        let jti_blk = format!("shr_blk_{blocked_id}");
        state
            .db
            .insert_share_link(
                &jti_blk,
                "export",
                blocked_id,
                None,
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let blocked_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: blocked_id.to_string(),
                exp: now + 3600,
                cam: None,
                jti: Some(jti_blk),
            },
        );
        for uri in [
            format!("/share/{blocked_tok}"),
            format!("/share/{blocked_tok}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::NOT_FOUND,
                "internal export fallback {blocked_id} must return 404 on {uri}"
            );
        }
    }

    // =========================================================================
    // 3. Creator Permission Coupling & Target/User Deletion Revocation (Finding 8)
    // =========================================================================
    // Initially `token` created by `op_lifecycle` (allowed: front_door,backyard) works (200 OK)
    for uri in [
        format!("/share/{token}"),
        format!("/share/{token}/video.mp4"),
    ] {
        let r = auth_app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    // Restrict `op_lifecycle` to only `backyard` -> `token` for `front_door` must return 403 FORBIDDEN
    state
        .db
        .update_user_allowed_cameras("op_lifecycle", "backyard")
        .unwrap();
    state.auth_gate.invalidate_user("op_lifecycle");
    for uri in [
        format!("/share/{token}"),
        format!("/share/{token}/video.mp4"),
    ] {
        let r = auth_app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::FORBIDDEN,
            "revoking creator camera permission must return 403 on {uri}"
        );
    }
    // Restore `op_lifecycle` camera permission -> 200 OK again
    state
        .db
        .update_user_allowed_cameras("op_lifecycle", "front_door,backyard")
        .unwrap();
    state.auth_gate.invalidate_user("op_lifecycle");

    // Helper to create a share link as a specific user
    let create_share = |hdr: String, kind: &str, target_id: &str| {
        let app = auth_app.clone();
        let body_str = json!({
            "kind": kind,
            "target_id": target_id,
            "expires_in_sec": 3600
        })
        .to_string();
        async move {
            let resp = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/share")
                        .header(header::AUTHORIZATION, hdr)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body_str))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let j: Value =
                serde_json::from_slice(&to_bytes(resp.into_body(), 4096).await.unwrap()).unwrap();
            (
                j["id"].as_str().unwrap().to_string(),
                j["token"].as_str().unwrap().to_string(),
            )
        }
    };

    // Single export delete revokes its share link
    let (jti_exp1, tok_exp1) = create_share(admin_hdr.clone(), "export", "exp_fd_1").await;
    let del_exp1 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/export/exp_fd_1")
                .header(header::AUTHORIZATION, &admin_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_exp1.status(), StatusCode::OK);
    assert!(state.db.is_share_link_revoked(&jti_exp1).unwrap());
    let r_exp1 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_exp1}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_exp1.status(), StatusCode::GONE);

    // Bulk export delete revokes its share link
    let (jti_exp2, tok_exp2) = create_share(admin_hdr.clone(), "export", "exp_fd_2").await;
    let del_exp2 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/exports/delete")
                .header(header::AUTHORIZATION, &admin_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ids":["exp_fd_2"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_exp2.status(), StatusCode::OK);
    assert!(state.db.is_share_link_revoked(&jti_exp2).unwrap());
    let r_exp2 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_exp2}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_exp2.status(), StatusCode::GONE);

    // Single event delete & bulk event delete revoke share links
    let (jti_ev1, tok_ev1) = create_share(admin_hdr.clone(), "event", "ev_p1_1").await;
    let (jti_ev2, tok_ev2) = create_share(admin_hdr.clone(), "event", "ev_p1_2").await;
    let del_ev1 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/events/ev_p1_1")
                .header(header::AUTHORIZATION, &admin_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_ev1.status(), StatusCode::OK);
    assert!(state.db.is_share_link_revoked(&jti_ev1).unwrap());
    assert_eq!(
        auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{tok_ev1}"))
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::GONE
    );

    let del_ev2 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/events")
                .header(header::AUTHORIZATION, &admin_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"event_ids":["ev_p1_2"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_ev2.status(), StatusCode::OK);
    assert!(state.db.is_share_link_revoked(&jti_ev2).unwrap());
    assert_eq!(
        auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{tok_ev2}/video.mp4"))
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::GONE
    );

    // Single review delete & bulk review delete revoke share links
    let (jti_rev1, tok_rev1) = create_share(admin_hdr.clone(), "review", "rev_p1_1").await;
    let (jti_rev2, tok_rev2) = create_share(admin_hdr.clone(), "review", "rev_p1_2").await;
    let del_rev1 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/review/rev_p1_1")
                .header(header::AUTHORIZATION, &admin_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_rev1.status(), StatusCode::OK);
    assert!(state.db.is_share_link_revoked(&jti_rev1).unwrap());
    assert_eq!(
        auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{tok_rev1}"))
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::GONE
    );

    let del_rev2 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/reviews/delete")
                .header(header::AUTHORIZATION, &admin_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ids":["rev_p1_2"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_rev2.status(), StatusCode::OK);
    assert!(state.db.is_share_link_revoked(&jti_rev2).unwrap());
    assert_eq!(
        auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{tok_rev2}/video.mp4"))
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::GONE
    );

    // User deletion via DELETE /api/users/{username} revokes all share links created by that user
    let (jti_user_del, tok_user_del) =
        create_share(op_del_hdr.clone(), "export", "exp_fd_keep").await;
    assert!(!state.db.is_share_link_revoked(&jti_user_del).unwrap());
    let resp_del_user = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/users/op_to_delete")
                .header(header::AUTHORIZATION, &admin_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_del_user.status(), StatusCode::OK);
    assert!(state.db.is_share_link_revoked(&jti_user_del).unwrap());
    for uri in [
        format!("/share/{tok_user_del}"),
        format!("/share/{tok_user_del}/video.mp4"),
    ] {
        let r = auth_app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::GONE);
    }

    // Finally, if the `share_link` table is unavailable/dropped, `POST /api/share`, `/share/{token}`,
    // and `/share/{token}/video.mp4` all fail closed with 500 INTERNAL_SERVER_ERROR
    state.db.execute_batch("DROP TABLE share_link;").unwrap();
    let resp_create_db_err = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &admin_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_fd_keep","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp_create_db_err.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );

    for uri in [
        format!("/share/{token}"),
        format!("/share/{token}/video.mp4"),
    ] {
        let r = auth_app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "DB error during JTI lookup must fail closed with 500 on {uri}"
        );
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_share_link_phase2_singleflight_mask_hash_multi_segment_and_path_containment_work()
{
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr_sec_share_phase2_{}",
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    let yaml = format!(
        r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let auth_app = build_router_with_auth(state.clone(), true);
    let now = unix_now();
    let admin_jwt = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &JwtClaims {
            sub: "admin".to_string(),
            role: "admin".to_string(),
            allowed_cameras: Some("*".to_string()),
            iat: now,
            exp: now + 3600,
        },
        &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .unwrap();
    let admin_hdr = format!("Bearer {admin_jwt}");

    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    let rec_fd_dir = media_dir.join("recordings/2026-11-03/12/front_door");
    let rec_by_dir = media_dir.join("recordings/2026-11-03/12/backyard");
    for d in [&exports_dir, &clips_dir, &rec_fd_dir, &rec_by_dir] {
        std::fs::create_dir_all(d).unwrap();
    }

    // Write a valid synthetic MP4 payload (>= 2KB)
    let mut mp4_payload = vec![0u8; 2048];
    mp4_payload[0..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p']);

    // =========================================================================
    // 1. Strict Path Containment Inside `media_dir` (Finding 4)
    // =========================================================================
    let outside_file = tmp_dir.join("outside_secret.mp4");
    std::fs::write(&outside_file, b"OUTSIDE_MEDIA_DIR_SECRET_BYTES").unwrap();
    let inside_file = exports_dir.join("inside_valid.mp4");
    std::fs::write(&inside_file, &mp4_payload).unwrap();

    assert!(ensure_path_within_media_dir(&media_dir, &inside_file).is_some());
    assert!(ensure_path_within_media_dir(&media_dir, &outside_file).is_none());
    let traversal_candidate = media_dir.join("exports/../../outside_secret.mp4");
    assert!(ensure_path_within_media_dir(&media_dir, &traversal_candidate).is_none());

    #[cfg(unix)]
    {
        let symlink_export = exports_dir.join("front_door_symlink_escape.mp4");
        std::os::unix::fs::symlink(&outside_file, &symlink_export).unwrap();
        assert!(ensure_path_within_media_dir(&media_dir, &symlink_export).is_none());

        // Disk-only export fallback pointing to a symlink outside media_dir must be rejected
        let (_, _, _, sym_exists) =
            resolve_share_target_metadata(&state, "export", "front_door_symlink_escape");
        assert!(
            !sym_exists,
            "symlink export escaping media_dir must not resolve as existing"
        );
        state
            .db
            .insert_share_link(
                "shr_sym_exp",
                "export",
                "front_door_symlink_escape",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let sym_exp_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: "front_door_symlink_escape".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some("shr_sym_exp".to_string()),
            },
        );
        let r_sym_exp = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{sym_exp_tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_sym_exp.status(), StatusCode::NOT_FOUND);

        // Recording symlink outside media_dir must be rejected
        let symlink_rec = rec_fd_dir.join("symlink_outside.mp4");
        std::os::unix::fs::symlink(&outside_file, &symlink_rec).unwrap();
        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: "rec_sym_escape".to_string(),
                camera: "front_door".to_string(),
                path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/symlink_outside.mp4"
                    .to_string(),
                start_time: 8000.0,
                end_time: 8010.0,
                duration: 10.0,
                motion: 1,
                objects: 1,
                dbfs: -20,
                segment_size: 0.1,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();
    }

    // Export row in DB with `..` traversal in video_path must be rejected
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_traversal".to_string(),
            camera: "front_door".to_string(),
            name: "Traversal Export".to_string(),
            date: 1700000100.0,
            video_path: "/media/ninjanvr/exports/../../outside_secret.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_traversal.webp".to_string(),
            in_progress: false,
        })
        .unwrap();
    state
        .db
        .insert_share_link(
            "shr_trav_exp",
            "export",
            "exp_traversal",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let trav_exp_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_traversal".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_trav_exp".to_string()),
        },
    );
    let r_trav_exp = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{trav_exp_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_trav_exp.status(), StatusCode::NOT_FOUND);

    // Event referencing recording with `..` traversal / symlink outside media_dir must return 404
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_trav_escape".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/../../../../../../etc/passwd".to_string(),
            start_time: 8000.0,
            end_time: 8010.0,
            duration: 10.0,
            motion: 1,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_escape_test".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 8002.0,
            end_time: Some(8008.0),
            has_clip: true,
            has_snapshot: true,
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
    state
        .db
        .insert_share_link(
            "shr_ev_escape",
            "event",
            "ev_escape_test",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let ev_escape_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "event".to_string(),
            id: "ev_escape_test".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_ev_escape".to_string()),
        },
    );
    let r_ev_escape = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ev_escape_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ev_escape.status(), StatusCode::NOT_FOUND);

    // =========================================================================
    // 2. Privacy Mask Hash Sidecar Invalidation & Singleflight Concurrency (Finding 1)
    // =========================================================================
    let seg_by_disk = rec_by_dir.join("00.00.mp4");
    std::fs::write(&seg_by_disk, b"UNREDACTED_BY_SEG").unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_by_p2_1".to_string(),
            camera: "backyard".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-03/12/backyard/00.00.mp4".to_string(),
            start_time: 2000.0,
            end_time: 2010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_p2_mask".to_string(),
            camera: "backyard".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 2001.0,
            end_time: Some(2008.0),
            has_clip: true,
            has_snapshot: true,
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

    let mask_v1 = json!([[[0.1, 0.1], [0.4, 0.1], [0.4, 0.4], [0.1, 0.4]]]);
    let mask_v2 = json!([[[0.2, 0.2], [0.8, 0.2], [0.8, 0.8], [0.2, 0.8]]]);
    assert_ne!(
        compute_privacy_mask_hash(Some(&mask_v1)),
        compute_privacy_mask_hash(Some(&mask_v2))
    );
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("backyard").unwrap().privacy_mask = mask_v1.clone();
    }

    let redacted_file = exports_dir.join("share_redacted_ev_p2_mask.mp4");
    let sidecar_file = exports_dir.join(".share_redacted_ev_p2_mask.mask_hash");
    std::fs::write(&redacted_file, b"REDACTED_V1_BYTES").unwrap();
    std::fs::write(&sidecar_file, compute_privacy_mask_hash(Some(&mask_v1))).unwrap();

    state
        .db
        .insert_share_link(
            "shr_ev_p2_mask",
            "event",
            "ev_p2_mask",
            Some("backyard"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let ev_p2_mask_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "event".to_string(),
            id: "ev_p2_mask".to_string(),
            exp: now + 3600,
            cam: Some("backyard".to_string()),
            jti: Some("shr_ev_p2_mask".to_string()),
        },
    );

    // First request with matching sidecar serves REDACTED_V1_BYTES
    let r_mask_v1 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ev_p2_mask_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_mask_v1.status(), StatusCode::OK);
    let v1_bytes = to_bytes(r_mask_v1.into_body(), 4096).await.unwrap();
    assert_eq!(&v1_bytes[..], b"REDACTED_V1_BYTES");
    assert_eq!(
        std::fs::read_to_string(&sidecar_file).unwrap().trim(),
        compute_privacy_mask_hash(Some(&mask_v1))
    );

    // Changing privacy_mask to mask_v2 invalidates the cached redacted clip, removes stale file, and rebuilds
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("backyard").unwrap().privacy_mask = mask_v2.clone();
    }
    let r_mask_v2 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ev_p2_mask_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Rebuild on non-transcodable synthetic bytes fails closed (404) and removes stale redacted_file + sidecar
    assert_eq!(r_mask_v2.status(), StatusCode::NOT_FOUND);
    assert!(
        !redacted_file.exists(),
        "stale redacted MP4 must be invalidated when privacy_mask changes"
    );
    assert!(
        !sidecar_file.exists(),
        "stale mask_hash sidecar must be removed when rebuild fails"
    );

    // Singleflight per-clip mutex: same clip key shares the same Arc<Mutex<()>>, distinct clip keys do not block each other
    let lock_a1 = acquire_share_clip_lock(&state, "share_redacted_ev_p2_mask").await;
    let lock_a2 = acquire_share_clip_lock(&state, "share_redacted_ev_p2_mask").await;
    let lock_b = acquire_share_clip_lock(&state, "share_clip_other").await;
    assert!(Arc::ptr_eq(&lock_a1, &lock_a2));
    assert!(!Arc::ptr_eq(&lock_a1, &lock_b));
    let guard_a = lock_a1.lock().await;
    assert!(
        lock_a2.try_lock().is_err(),
        "concurrent singleflight lock on same clip must wait"
    );
    assert!(
        lock_b.try_lock().is_ok(),
        "different clip must not be blocked by lock_a"
    );
    drop(guard_a);

    // =========================================================================
    // 3. Bounded Window, Multi-Segment Concatenation (`share_clip_`), & Static Media Camera Scoping (Finding 2)
    // =========================================================================
    let seg_fd_1 = rec_fd_dir.join("00.00.mp4");
    let seg_fd_2 = rec_fd_dir.join("00.10.mp4");
    std::fs::write(&seg_fd_1, &mp4_payload).unwrap();
    std::fs::write(&seg_fd_2, &mp4_payload).unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_fd_multi_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/00.00.mp4".to_string(),
            start_time: 3000.0,
            end_time: 3010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_fd_multi_2".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/00.10.mp4".to_string(),
            start_time: 3010.0,
            end_time: 3020.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_fd_multi".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 3002.0,
            end_time: Some(3018.0),
            has_clip: true,
            has_snapshot: true,
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

    state
        .db
        .insert_share_link(
            "shr_ev_fd_multi",
            "event",
            "ev_fd_multi",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let ev_fd_multi_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "event".to_string(),
            id: "ev_fd_multi".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_ev_fd_multi".to_string()),
        },
    );
    // Spawn two concurrent requests for multi-segment unmasked clip to exercise singleflight + build_mp4_export_with_privacy
    let app_c1 = auth_app.clone();
    let app_c2 = auth_app.clone();
    let tok_c1 = ev_fd_multi_tok.clone();
    let tok_c2 = ev_fd_multi_tok.clone();
    let (resp_m1, resp_m2) = tokio::join!(
        app_c1.oneshot(
            Request::builder()
                .uri(format!("/share/{tok_c1}/video.mp4"))
                .body(Body::empty())
                .unwrap()
        ),
        app_c2.oneshot(
            Request::builder()
                .uri(format!("/share/{tok_c2}/video.mp4"))
                .body(Body::empty())
                .unwrap()
        )
    );
    assert_eq!(resp_m1.unwrap().status(), StatusCode::OK);
    assert_eq!(resp_m2.unwrap().status(), StatusCode::OK);
    assert!(
        exports_dir.join("share_clip_ev_fd_multi.mp4").is_file(),
        "multi-segment unmasked share must build exports/share_clip_<id>.mp4"
    );
    assert!(
        state
            .db
            .get_export_by_id("share_clip_ev_fd_multi")
            .unwrap()
            .is_none(),
        "transient share_clip_ DB export row must be deleted"
    );
    assert!(
        !clips_dir.join("share_clip_ev_fd_multi.webp").exists(),
        "transient share_clip_ thumbnail must be cleaned up"
    );

    // Verify resolve_static_media_camera strips "share_clip_" alongside "share_redacted_"
    assert_eq!(
        resolve_static_media_camera(&state, "/exports/share_clip_ev_fd_multi.mp4").as_deref(),
        Some("front_door")
    );
    assert_eq!(
        resolve_static_media_camera(&state, "/exports/share_clip_ev_p2_mask.mp4").as_deref(),
        Some("backyard")
    );
    assert_eq!(
        resolve_static_media_camera(&state, "/clips/share_clip_ev_fd_multi.webp").as_deref(),
        Some("front_door")
    );

    // =========================================================================
    // 4. Artifact Cleanup on Share Link Revocation and Event/Review Deletion (Finding 1)
    // =========================================================================
    let seed_artifacts = |target_id: &str| {
        let r_mp4 = exports_dir.join(format!("share_redacted_{target_id}.mp4"));
        let r_hash = exports_dir.join(format!(".share_redacted_{target_id}.mask_hash"));
        let c_mp4 = exports_dir.join(format!("share_clip_{target_id}.mp4"));
        std::fs::write(&r_mp4, b"R").unwrap();
        std::fs::write(&r_hash, b"H").unwrap();
        std::fs::write(&c_mp4, b"C").unwrap();
        (r_mp4, r_hash, c_mp4)
    };

    // 4a. Revoking one share link while a sibling link (`shr_ev_fd_multi`) is still active preserves shared artifacts;
    //     revoking the last active share link removes its target's share_redacted_*, .mask_hash, and share_clip_* artifacts
    state
        .db
        .insert_share_link(
            "shr_cleanup_revoke",
            "event",
            "ev_fd_multi",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let (r_mp4, r_hash, c_mp4) = seed_artifacts("ev_fd_multi");
    let resp_rev = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/share/shr_cleanup_revoke")
                .header(header::AUTHORIZATION, &admin_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev.status(), StatusCode::OK);
    assert!(
        r_mp4.exists() && r_hash.exists() && c_mp4.exists(),
        "shared artifacts must be preserved while sibling link shr_ev_fd_multi is still active"
    );
    let resp_rev_last = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/share/shr_ev_fd_multi")
                .header(header::AUTHORIZATION, &admin_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_last.status(), StatusCode::OK);
    assert!(!r_mp4.exists() && !r_hash.exists() && !c_mp4.exists());

    // 4b. Deleting an event removes its share_redacted_*, .mask_hash, and share_clip_* artifacts
    let (r_mp4, r_hash, c_mp4) = seed_artifacts("ev_fd_multi");
    let resp_del_ev = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/events/ev_fd_multi")
                .header(header::AUTHORIZATION, &admin_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_del_ev.status(), StatusCode::OK);
    assert!(!r_mp4.exists() && !r_hash.exists() && !c_mp4.exists());

    // 4c. Deleting a review removes its share_redacted_*, .mask_hash, and share_clip_* artifacts
    state
        .db
        .insert_review_segment(&ninjanvr_db::ReviewItemDto {
            id: "rev_p2_clean".to_string(),
            camera: "front_door".to_string(),
            start_time: 3000.0,
            end_time: Some(3010.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-front_door-rev_p2_clean.webp"
                .to_string(),
            data: json!({}),
            has_been_reviewed: false,
        })
        .unwrap();
    let (r_mp4, r_hash, c_mp4) = seed_artifacts("rev_p2_clean");
    let resp_del_rev = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/review/rev_p2_clean")
                .header(header::AUTHORIZATION, &admin_hdr)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_del_rev.status(), StatusCode::OK);
    assert!(!r_mp4.exists() && !r_hash.exists() && !c_mp4.exists());

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_share_link_phase3_corp_csrf_max_views_and_audit_tracking_work() {
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr_sec_share_phase3_{}",
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    let yaml = format!(
        r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let auth_app = build_router_with_auth(state.clone(), true);

    // Verify .jwt_secret was atomically created with mode 0o600
    let jwt_secret_path = tmp_dir.join(".jwt_secret");
    assert!(jwt_secret_path.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&jwt_secret_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    let now = unix_now();
    let admin_jwt = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &JwtClaims {
            sub: "admin".to_string(),
            role: "admin".to_string(),
            allowed_cameras: Some("*".to_string()),
            iat: now,
            exp: now + 3600,
        },
        &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .unwrap();
    let admin_bearer = format!("Bearer {admin_jwt}");
    let admin_cookie = format!("ninjanvr_token={admin_jwt}");

    let exports_dir = media_dir.join("exports");
    std::fs::create_dir_all(&exports_dir).unwrap();
    let mp4_payload: Vec<u8> = (0u8..64u8).collect();
    let exp_file = exports_dir.join("exp_p3_1.mp4");
    std::fs::write(&exp_file, &mp4_payload).unwrap();
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_p3_1".to_string(),
            camera: "front_door".to_string(),
            name: "Phase 3 Export".to_string(),
            date: 1700000100.0,
            video_path: "/media/ninjanvr/exports/exp_p3_1.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_p3_1.webp".to_string(),
            in_progress: false,
        })
        .unwrap();

    // =========================================================================
    // 1. Cross-Origin-Resource-Policy: same-origin on bytes/file range helpers
    // =========================================================================
    let empty_hdrs = HeaderMap::new();
    let mut range_hdrs = HeaderMap::new();
    range_hdrs.insert(header::RANGE, "bytes=0-15".parse().unwrap());

    let bytes_200 = serve_mp4_bytes_with_range(mp4_payload.clone(), &empty_hdrs);
    assert_eq!(
        bytes_200
            .headers()
            .get("cross-origin-resource-policy")
            .unwrap(),
        "same-origin"
    );
    let bytes_206 = serve_mp4_bytes_with_range(mp4_payload.clone(), &range_hdrs);
    assert_eq!(
        bytes_206
            .headers()
            .get("cross-origin-resource-policy")
            .unwrap(),
        "same-origin"
    );

    let file_200 = serve_mp4_file_with_range(&exp_file, &empty_hdrs)
        .await
        .unwrap();
    assert_eq!(
        file_200
            .headers()
            .get("cross-origin-resource-policy")
            .unwrap(),
        "same-origin"
    );
    let file_206 = serve_mp4_file_with_range(&exp_file, &range_hdrs)
        .await
        .unwrap();
    assert_eq!(
        file_206
            .headers()
            .get("cross-origin-resource-policy")
            .unwrap(),
        "same-origin"
    );

    // =========================================================================
    // 2. Cookie CSRF Origin Guard on POST /api/share and DELETE /api/share/{id}
    // =========================================================================
    // 2a. Cookie auth + mismatched Origin -> 403 FORBIDDEN on POST /api/share
    for (origin_val, host_opt) in [
        ("https://evil.example", Some("nvr.local:5000")),
        ("http://nvr.local:8971", Some("nvr.local:5000")),
        ("null", Some("nvr.local:5000")),
        ("https://nvr.local:5000", None),
    ] {
        let mut req_b = Request::builder()
            .method("POST")
            .uri("/api/share")
            .header(header::COOKIE, &admin_cookie)
            .header(header::ORIGIN, origin_val)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(h) = host_opt {
            req_b = req_b.header(header::HOST, h);
        }
        let r = auth_app
            .clone()
            .oneshot(
                req_b
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p3_1","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::FORBIDDEN,
            "cookie POST /api/share with Origin={origin_val} Host={host_opt:?} must return 403"
        );
    }

    // 2b. Cookie auth + matching Origin (scheme-agnostic host:port) -> 200 OK
    let resp_cookie_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::COOKIE, &admin_cookie)
                .header(header::ORIGIN, "https://nvr.local:5000")
                .header(header::HOST, "nvr.local:5000")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_p3_1","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_cookie_ok.status(), StatusCode::OK);
    let cookie_share_json: Value =
        serde_json::from_slice(&to_bytes(resp_cookie_ok.into_body(), 4096).await.unwrap()).unwrap();
    let cookie_share_id = cookie_share_json["id"].as_str().unwrap().to_string();
    assert!(cookie_share_json.get("max_views").is_none());

    // 2c. Cookie auth + mismatched Origin on DELETE /api/share/{id} -> 403 FORBIDDEN (link stays unrevoked)
    let resp_del_csrf = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/share/{cookie_share_id}"))
                .header(header::COOKIE, &admin_cookie)
                .header(header::ORIGIN, "https://evil.example")
                .header(header::HOST, "nvr.local:5000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_del_csrf.status(), StatusCode::FORBIDDEN);
    assert!(!state.db.is_share_link_revoked(&cookie_share_id).unwrap());

    // 2d. Cookie auth + matching Origin on DELETE /api/share/{id} -> 200 OK
    let resp_del_cookie_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/share/{cookie_share_id}"))
                .header(header::COOKIE, &admin_cookie)
                .header(header::ORIGIN, "http://nvr.local:5000")
                .header(header::HOST, "nvr.local:5000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_del_cookie_ok.status(), StatusCode::OK);
    assert!(state.db.is_share_link_revoked(&cookie_share_id).unwrap());

    // 2e. HTTP/2 `:authority` (no `Host` header) and reverse-proxy `X-Forwarded-Host`
    //     must succeed for same-origin requests and reject cross-origin requests.
    let resp_h2_create = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("https://nvr.example.test/api/share")
                .header(header::COOKIE, &admin_cookie)
                .header(header::ORIGIN, "https://nvr.example.test")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_p3_1","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_h2_create.status(), StatusCode::OK);
    let h2_share_json: Value =
        serde_json::from_slice(&to_bytes(resp_h2_create.into_body(), 4096).await.unwrap()).unwrap();
    let h2_share_id = h2_share_json["id"].as_str().unwrap().to_string();

    // Cross-origin against HTTP/2 `:authority` -> 403 FORBIDDEN
    let resp_h2_csrf = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("https://nvr.example.test/api/share/{h2_share_id}"))
                .header(header::COOKIE, &admin_cookie)
                .header(header::ORIGIN, "https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_h2_csrf.status(), StatusCode::FORBIDDEN);
    assert!(!state.db.is_share_link_revoked(&h2_share_id).unwrap());

    // Forged `X-Forwarded-Host: evil.example` against an external `Host` is rejected -> 403 FORBIDDEN
    let resp_forged_xfh = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/share/{h2_share_id}"))
                .header(header::COOKIE, &admin_cookie)
                .header(header::ORIGIN, "https://evil.example")
                .header("x-forwarded-host", "evil.example")
                .header(header::HOST, "nvr.example.test:5000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_forged_xfh.status(), StatusCode::FORBIDDEN);
    assert!(!state.db.is_share_link_revoked(&h2_share_id).unwrap());

    // Reverse-proxy `X-Forwarded-Host` with internal `Host` and default `:443` port -> 200 OK
    let resp_xfh_revoke = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/share/{h2_share_id}"))
                .header(header::COOKIE, &admin_cookie)
                .header(header::ORIGIN, "https://nvr.example.test")
                .header("x-forwarded-host", "nvr.example.test:443, proxy.internal")
                .header(header::HOST, "127.0.0.1:8971")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_xfh_revoke.status(), StatusCode::OK);
    assert!(state.db.is_share_link_revoked(&h2_share_id).unwrap());

    // =========================================================================
    // 3. Max-Views Enforcement, Access Counter, and Last-Accessed Timestamp
    // =========================================================================
    // Create a link with max_views: 2 (Bearer auth works even with cross-origin Origin header)
    let resp_mv2 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::ORIGIN, "https://cli-client.example")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p3_1","expires_in_sec":3600,"max_views":2}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp_mv2.status(), StatusCode::OK);
    let mv2_json: Value =
        serde_json::from_slice(&to_bytes(resp_mv2.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(mv2_json["max_views"], 2);
    let mv2_id = mv2_json["id"].as_str().unwrap().to_string();
    let mv2_tok = mv2_json["token"].as_str().unwrap().to_string();

    let db_before = state.db.get_share_link(&mv2_id).unwrap().unwrap();
    assert_eq!(db_before["access_count"], 0);
    assert!(db_before["last_accessed_at"].is_null());
    assert_eq!(db_before["max_views"], 2);

    // View 1: GET /share/{token} (share_player_page) -> 200 OK + CORP: same-origin, access_count = 1
    let v1_resp = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{mv2_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(v1_resp.status(), StatusCode::OK);
    assert_eq!(
        v1_resp
            .headers()
            .get("cross-origin-resource-policy")
            .unwrap(),
        "same-origin"
    );
    let db_v1 = state.db.get_share_link(&mv2_id).unwrap().unwrap();
    assert_eq!(db_v1["access_count"], 1);
    assert!(db_v1["last_accessed_at"].as_f64().unwrap() >= now as f64);

    // Embedded <video> initial request after View 1 player page consumes the page credit (access_count stays 1)
    let v1_video_resp = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{mv2_tok}/video.mp4"))
                .header(header::RANGE, "bytes=0-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(v1_video_resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        state.db.get_share_link(&mv2_id).unwrap().unwrap()["access_count"],
        1
    );

    // View 2: Standalone initial GET /share/{token}/video.mp4 with Range: bytes=0-15 -> 206 Partial Content, access_count = 2
    let v2_resp = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{mv2_tok}/video.mp4"))
                .header(header::RANGE, "bytes=0-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(v2_resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        v2_resp
            .headers()
            .get("cross-origin-resource-policy")
            .unwrap(),
        "same-origin"
    );
    let db_v2 = state.db.get_share_link(&mv2_id).unwrap().unwrap();
    assert_eq!(db_v2["access_count"], 2);

    // Mid-stream chunk: Range: bytes=16-63 succeeds (206) without incrementing access_count beyond 2
    let mid_resp = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{mv2_tok}/video.mp4"))
                .header(header::RANGE, "bytes=16-63")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mid_resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        state.db.get_share_link(&mv2_id).unwrap().unwrap()["access_count"],
        2
    );

    // View 3 (exceeding max_views = 2): both /share/{token} and /share/{token}/video.mp4 return 410 GONE
    for uri in [
        format!("/share/{mv2_tok}"),
        format!("/share/{mv2_tok}/video.mp4"),
    ] {
        let r = auth_app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::GONE,
            "exhausted max_views link must return 410 GONE on {uri}"
        );
    }
    assert_eq!(
        state.db.get_share_link(&mv2_id).unwrap().unwrap()["access_count"],
        2
    );

    // Single-use link (max_views: 0 clamped to 1) end-to-end player + embedded <video> playback
    let resp_mv0 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p3_1","expires_in_sec":3600,"max_views":0}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp_mv0.status(), StatusCode::OK);
    let mv0_json: Value =
        serde_json::from_slice(&to_bytes(resp_mv0.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(mv0_json["max_views"], 1);
    let mv1_id = mv0_json["id"].as_str().unwrap().to_string();
    let mv1_tok = mv0_json["token"].as_str().unwrap().to_string();

    // 1) GET /share/{token} succeeds (200 OK) and sets access_count == 1
    let mv1_page = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{mv1_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mv1_page.status(), StatusCode::OK);
    assert_eq!(
        state.db.get_share_link(&mv1_id).unwrap().unwrap()["access_count"],
        1
    );

    // 2) Embedded <video> initial request (Range: bytes=0-15) and mid-stream chunk (Range: bytes=16-63) succeed with access_count == 1
    for range_spec in ["bytes=0-15", "bytes=16-63"] {
        let r = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{mv1_tok}/video.mp4"))
                    .header(header::RANGE, range_spec)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::PARTIAL_CONTENT,
            "max_views=1 embedded video request with {range_spec} must succeed"
        );
        assert_eq!(
            state.db.get_share_link(&mv1_id).unwrap().unwrap()["access_count"],
            1
        );
    }

    // 3) Subsequent second GET /share/{token} and subsequent second initial GET /share/{token}/video.mp4 both return 410 GONE
    for uri in [
        format!("/share/{mv1_tok}"),
        format!("/share/{mv1_tok}/video.mp4"),
    ] {
        let r = auth_app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::GONE,
            "second visit to max_views=1 link must return 410 GONE on {uri}"
        );
    }
    let r_range0_again = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{mv1_tok}/video.mp4"))
                .header(header::RANGE, "bytes=0-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_range0_again.status(), StatusCode::GONE);

    // 4) [CRITICO-01] Range: bytes=1- bypass attempt after max_views exhaustion is rejected with 410 GONE
    for range_bypass in ["bytes=1-", "bytes=1-31", "bytes=16-63"] {
        let r_bypass = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{mv1_tok}/video.mp4"))
                    .header(header::RANGE, range_bypass)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_bypass.status(),
            StatusCode::GONE,
            "Range={range_bypass} after max_views exhaustion must return 410 GONE"
        );
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_share_link_p0_p1_range_bypass_csrf_jti_creator_and_bot_protection_work() {
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr_sec_share_p0_p1_{}",
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
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
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let auth_app = build_router_with_auth(state.clone(), true);

    let now = unix_now();
    let make_jwt = |sub: &str, role: &str, allowed_cameras: Option<&str>| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: allowed_cameras.map(str::to_string),
                iat: unix_now(),
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };

    let admin_jwt = make_jwt("admin", "admin", Some("*"));
    let admin_bearer = format!("Bearer {admin_jwt}");
    let admin_cookie = format!("ninjanvr_token={admin_jwt}");

    let exports_dir = media_dir.join("exports");
    std::fs::create_dir_all(&exports_dir).unwrap();
    let mp4_payload: Vec<u8> = (0u8..64u8).collect();
    std::fs::write(exports_dir.join("exp_p0p1.mp4"), &mp4_payload).unwrap();
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_p0p1".to_string(),
            camera: "front_door".to_string(),
            name: "P0 P1 Export".to_string(),
            date: 1700000100.0,
            video_path: "/media/ninjanvr/exports/exp_p0p1.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_p0p1.webp".to_string(),
            in_progress: false,
        })
        .unwrap();

    // =========================================================================
    // 1. [CRITICO-01] Range: bytes=1- max_views bypass protection
    // =========================================================================
    // 1a. Sending Range: bytes=1- as the very first request on a max_views=1 link counts as the 1 view,
    //     and a second Range: bytes=1- is rejected with 410 GONE!
    let resp_create_r1 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp_create_r1.status(), StatusCode::OK);
    let r1_json: Value =
        serde_json::from_slice(&to_bytes(resp_create_r1.into_body(), 4096).await.unwrap()).unwrap();
    let r1_id = r1_json["id"].as_str().unwrap().to_string();
    let r1_tok = r1_json["token"].as_str().unwrap().to_string();

    let first_range1 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{r1_tok}/video.mp4"))
                .header(header::RANGE, "bytes=1-")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first_range1.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        state.db.get_share_link(&r1_id).unwrap().unwrap()["access_count"],
        1
    );

    let second_range1 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{r1_tok}/video.mp4"))
                .header(header::RANGE, "bytes=1-")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second_range1.status(), StatusCode::GONE);

    // 1b. Consuming max_views=1 via player page (GET /share/{tok}) and then immediately sending
    //     Range: bytes=1-, Range: bytes=2-, or Range: bytes=16-63 (without an initial Range: bytes=0-...)
    //     is rejected with 410 GONE, and the legitimate Range: bytes=0-15 still works afterwards using
    //     the unconsumed page credit!
    let resp_create_r2 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let r2_json: Value =
        serde_json::from_slice(&to_bytes(resp_create_r2.into_body(), 4096).await.unwrap()).unwrap();
    let r2_id = r2_json["id"].as_str().unwrap().to_string();
    let r2_tok = r2_json["token"].as_str().unwrap().to_string();

    let r2_page = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{r2_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2_page.status(), StatusCode::OK);

    for premature_range in ["bytes=1-", "bytes=2-", "bytes=16-63"] {
        let r2_premature = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{r2_tok}/video.mp4"))
                    .header(header::RANGE, premature_range)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r2_premature.status(),
            StatusCode::GONE,
            "Range: {premature_range} directly after player page when max_views=1 must return 410 GONE"
        );
    }

    let r2_legit_initial = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{r2_tok}/video.mp4"))
                .header(header::RANGE, "bytes=0-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r2_legit_initial.status(),
        StatusCode::PARTIAL_CONTENT,
        "legitimate Range: bytes=0-15 after rejected premature non-zero ranges must succeed using unconsumed page credit"
    );
    assert_eq!(
        state.db.get_share_link(&r2_id).unwrap().unwrap()["access_count"],
        1
    );

    // 1c. An initial open-ended Range: bytes=0- on a max_views=1 link followed by Range: bytes=16-63
    //     succeeds (206 PARTIAL_CONTENT, access_count == 1), while a subsequent second viewer attempting
    //     Range: bytes=0- and then Range: bytes=1- or Range: bytes=16-63 gets 410 GONE on all of them!
    let resp_create_r3 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let r3_json: Value =
        serde_json::from_slice(&to_bytes(resp_create_r3.into_body(), 4096).await.unwrap()).unwrap();
    let r3_id = r3_json["id"].as_str().unwrap().to_string();
    let r3_tok = r3_json["token"].as_str().unwrap().to_string();

    for viewer1_range in ["bytes=0-", "bytes=16-63"] {
        let r3_v1 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{r3_tok}/video.mp4"))
                    .header(header::RANGE, viewer1_range)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r3_v1.status(),
            StatusCode::PARTIAL_CONTENT,
            "viewer 1 request with Range: {viewer1_range} must return 206 PARTIAL_CONTENT"
        );
        assert_eq!(
            state.db.get_share_link(&r3_id).unwrap().unwrap()["access_count"],
            1
        );
    }

    for viewer2_range in ["bytes=0-", "bytes=1-", "bytes=16-63"] {
        let r3_v2 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{r3_tok}/video.mp4"))
                    .header(header::RANGE, viewer2_range)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r3_v2.status(),
            StatusCode::GONE,
            "viewer 2 request with Range: {viewer2_range} on exhausted max_views=1 link must return 410 GONE"
        );
    }

    // =========================================================================
    // 2. [ALTO-02] CSRF Parser Differential, Sec-Fetch-Site: cross-site, and Referer
    // =========================================================================
    // 2a. Sec-Fetch-Site: cross-site rejected even when Origin is absent
    let resp_sfs = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::COOKIE, &admin_cookie)
                .header("sec-fetch-site", "cross-site")
                .header(header::HOST, "nvr.local:5000")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_sfs.status(), StatusCode::FORBIDDEN);

    // 2b. Mismatched or malformed Referer when Origin is absent -> 403 FORBIDDEN; matching Referer -> 200 OK
    for bad_referer in [
        "https://evil.example/csrf.html",
        "http://nvr.local:8971/page",
        "malformed_no_scheme",
        "://missing_scheme",
        "https:///missing_host",
    ] {
        let r = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::COOKIE, &admin_cookie)
                    .header(header::REFERER, bad_referer)
                    .header(header::HOST, "nvr.local:5000")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::FORBIDDEN,
            "bad Referer={bad_referer} without Origin must return 403"
        );
    }

    let resp_good_referer = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::COOKIE, &admin_cookie)
                .header(header::REFERER, "https://nvr.local:5000/exports?filter=1")
                .header(header::HOST, "nvr.local:5000")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_good_referer.status(), StatusCode::OK);

    // 2c. No fallback to session cookie when an invalid/lowercase/whitespace Authorization header is sent!
    for bad_auth in [
        "bearer invalid_token",
        "  Bearer invalid_token",
        "Bearer invalid_token",
        "Basic dXNlcjpwYXNz",
        "Bearer   ",
    ] {
        let r = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::COOKIE, &admin_cookie)
                    .header(header::AUTHORIZATION, bad_auth)
                    .header(header::ORIGIN, "https://evil.example")
                    .header(header::HOST, "nvr.local:5000")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            r.status() == StatusCode::UNAUTHORIZED || r.status() == StatusCode::FORBIDDEN,
            "invalid Authorization={bad_auth:?} must NOT fall back to session cookie (got {})",
            r.status()
        );
    }

    // Valid lowercase `bearer <jwt>` with leading whitespace succeeds
    let resp_lower_bearer = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, format!("  bearer {admin_jwt}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_lower_bearer.status(), StatusCode::OK);

    // Multi-byte UTF-8 input in parse_authorization_bearer never panics on char boundaries
    for utf8_input in ["🔥earer tok", "bear🔥r tok", "🔥🔥 tok", "  🔥bearer tok"] {
        assert_eq!(parse_authorization_bearer(utf8_input), None);
    }

    // =========================================================================
    // 3. [ALTO-03] Mandatory JTI Enforcement (jti: None or empty -> 403 FORBIDDEN)
    // =========================================================================
    for missing_jti in [None, Some(String::new()), Some("   ".to_string())] {
        let no_jti_tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: "exp_p0p1".to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: missing_jti.clone(),
            },
        );
        for uri in [
            format!("/share/{no_jti_tok}"),
            format!("/share/{no_jti_tok}/video.mp4"),
        ] {
            let r = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::FORBIDDEN,
                "token with jti={missing_jti:?} must return 403 FORBIDDEN on {uri}"
            );
        }
    }

    // =========================================================================
    // 4. [ALTO-04] Creator Lifecycle: Role Demotion to Viewer & API Token Revocation/Expiration
    // =========================================================================
    state
        .db
        .create_user_with_cameras("op_demote", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    let op_demote_hdr = format!(
        "Bearer {}",
        make_jwt("op_demote", "operator", Some("front_door"))
    );
    let resp_op_share = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &op_demote_hdr)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_op_share.status(), StatusCode::OK);
    let op_share_json: Value =
        serde_json::from_slice(&to_bytes(resp_op_share.into_body(), 4096).await.unwrap()).unwrap();
    let op_share_id = op_share_json["id"].as_str().unwrap().to_string();
    let op_share_tok = op_share_json["token"].as_str().unwrap().to_string();

    // 4a. Direct DB role demotion to viewer -> 403 FORBIDDEN in verify_share_jti_with_view_mode
    state
        .db
        .execute_batch("UPDATE user SET role = 'viewer' WHERE username = 'op_demote';")
        .unwrap();
    for uri in [
        format!("/share/{op_share_tok}"),
        format!("/share/{op_share_tok}/video.mp4"),
    ] {
        let r = auth_app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    // Restore to operator, then demote via PUT /api/users/op_demote/role -> revokes link in DB (410 GONE)
    state.db.update_user_role("op_demote", "operator").unwrap();
    state.auth_gate.invalidate_user("op_demote");
    let resp_api_demote = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/users/op_demote/role")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"role":"viewer"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_api_demote.status(), StatusCode::OK);
    assert!(state.db.is_share_link_revoked(&op_share_id).unwrap());
    let r_after_demote = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{op_share_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_after_demote.status(), StatusCode::GONE);

    // 4b. Creator user deleted directly in DB -> 403 FORBIDDEN (fail-closed)
    state
        .db
        .insert_share_link(
            "shr_ghost_creator",
            "export",
            "exp_p0p1",
            Some("front_door"),
            "nonexistent_creator_user",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let ghost_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_p0p1".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_ghost_creator".to_string()),
        },
    );
    for uri in [
        format!("/share/{ghost_tok}"),
        format!("/share/{ghost_tok}/video.mp4"),
    ] {
        let r = auth_app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    // 4c. Share link created by full-scope API token: works while valid, blocked when token expired or deleted
    let (tok_meta, tok_plain) = state
        .db
        .create_api_token("ci_share_bot", "full", Some((now + 3600) as f64))
        .unwrap();
    let api_tok_id = tok_meta["id"].as_str().unwrap().to_string();
    let resp_tok_share = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, format!("Bearer {tok_plain}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_tok_share.status(), StatusCode::OK);
    let tok_share_json: Value =
        serde_json::from_slice(&to_bytes(resp_tok_share.into_body(), 4096).await.unwrap()).unwrap();
    let tok_share_jti = tok_share_json["id"].as_str().unwrap().to_string();
    let tok_share_token = tok_share_json["token"].as_str().unwrap().to_string();

    // Works while API token is active
    let r_tok_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_share_token}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_tok_ok.status(), StatusCode::OK);

    // Deleting the API token via DELETE /api/tokens/{id} revokes its share links in DB (410 GONE)
    let resp_del_apitok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/tokens/{api_tok_id}"))
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_del_apitok.status(), StatusCode::OK);
    assert!(state.db.is_share_link_revoked(&tok_share_jti).unwrap());
    let r_after_tok_del = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_share_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_after_tok_del.status(), StatusCode::GONE);

    // Expired API token creator in verify_share_jti_with_view_mode -> 403 FORBIDDEN
    state
        .db
        .create_api_token("expired_share_bot", "full", Some((now - 10) as f64))
        .unwrap();
    state
        .db
        .insert_share_link(
            "shr_expired_apitok",
            "export",
            "exp_p0p1",
            Some("front_door"),
            "token:expired_share_bot",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let exp_apitok_share = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_p0p1".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_expired_apitok".to_string()),
        },
    );
    for uri in [
        format!("/share/{exp_apitok_share}"),
        format!("/share/{exp_apitok_share}/video.mp4"),
    ] {
        let r = auth_app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    // =========================================================================
    // 5. [BAIXO-09] Chat Link-Preview Bot Protection & Robots Meta Tag
    // =========================================================================
    let resp_bot_share = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_p0p1","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let bot_share_json: Value =
        serde_json::from_slice(&to_bytes(resp_bot_share.into_body(), 4096).await.unwrap()).unwrap();
    let bot_share_id = bot_share_json["id"].as_str().unwrap().to_string();
    let bot_share_tok = bot_share_json["token"].as_str().unwrap().to_string();

    for bot_ua in [
        "WhatsApp/2.23.20.0",
        "Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)",
        "TelegramBot (like TwitterBot)",
        "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)",
        "Twitterbot/1.0",
        "facebookexternalhit/1.1",
        "LinkedInBot/1.0",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X) Applebot/0.1",
    ] {
        let r_bot = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{bot_share_tok}"))
                    .header(header::USER_AGENT, bot_ua)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_bot.status(),
            StatusCode::OK,
            "bot UA {bot_ua} must get 200 OK"
        );
        let bot_html =
            String::from_utf8(to_bytes(r_bot.into_body(), 8192).await.unwrap().to_vec()).unwrap();
        assert!(
            bot_html.contains(r#"<meta name="robots" content="noindex, nofollow">"#),
            "bot preview HTML must include noindex, nofollow meta tag"
        );
        assert!(
            !bot_html.contains("<video"),
            "bot preview HTML must not embed autoplay <video>"
        );
        assert_eq!(
            state.db.get_share_link(&bot_share_id).unwrap().unwrap()["access_count"],
            0,
            "bot UA {bot_ua} must NOT consume access_count"
        );
    }

    // Human recipient clicking the max_views=1 link after all chat bots still gets the view!
    let r_human = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{bot_share_tok}"))
                .header(
                    header::USER_AGENT,
                    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/130.0.0.0",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_human.status(), StatusCode::OK);
    let human_html =
        String::from_utf8(to_bytes(r_human.into_body(), 16384).await.unwrap().to_vec()).unwrap();
    assert!(
        human_html.contains(r#"<meta name="robots" content="noindex, nofollow">"#),
        "human player page HTML must include noindex, nofollow meta tag"
    );
    assert!(human_html.contains("<video"));
    assert_eq!(
        state.db.get_share_link(&bot_share_id).unwrap().unwrap()["access_count"],
        1
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_share_link_p2_media_privacy_dos_and_static_artifact_protection_work() {
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr_sec_share_p2_media_dos_{}",
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
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
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let auth_app = build_router_with_auth(state.clone(), true);
    let unauth_app = build_router(state.clone());

    let now = unix_now();
    let make_jwt = |sub: &str, role: &str, allowed_cameras: Option<&str>| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: allowed_cameras.map(str::to_string),
                iat: unix_now(),
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };

    state
        .db
        .create_user_with_cameras("scoped_fd_op", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    let admin_bearer = format!("Bearer {}", make_jwt("admin", "admin", Some("*")));
    let scoped_op_bearer = format!(
        "Bearer {}",
        make_jwt("scoped_fd_op", "operator", Some("front_door"))
    );

    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    let rec_fd_dir = media_dir.join("recordings/2026-11-03/12/front_door");
    let rec_by_dir = media_dir.join("recordings/2026-11-03/12/backyard");
    for d in [&exports_dir, &clips_dir, &rec_fd_dir, &rec_by_dir] {
        std::fs::create_dir_all(d).unwrap();
    }

    // =========================================================================
    // 1. [ALTO-05] Export with Active Camera Privacy Mask Redacts via share_redacted_<id>.mp4
    // =========================================================================
    let raw_export_bytes = b"UNREDACTED_RAW_EXPORT_STREAM_BYTES".to_vec();
    std::fs::write(exports_dir.join("exp_by_masked.mp4"), &raw_export_bytes).unwrap();
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_by_masked".to_string(),
            camera: "backyard".to_string(),
            name: "Backyard Export".to_string(),
            date: 1700000100.0,
            video_path: "/media/ninjanvr/exports/exp_by_masked.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_by_masked.webp".to_string(),
            in_progress: false,
        })
        .unwrap();
    state
        .db
        .insert_share_link(
            "shr_exp_by_masked",
            "export",
            "exp_by_masked",
            Some("backyard"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let exp_by_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_by_masked".to_string(),
            exp: now + 3600,
            cam: Some("backyard".to_string()),
            jti: Some("shr_exp_by_masked".to_string()),
        },
    );

    let mask_v1 = json!([[[0.1, 0.1], [0.4, 0.1], [0.4, 0.4], [0.1, 0.4]]]);
    let mask_v2 = json!([[[0.2, 0.2], [0.8, 0.2], [0.8, 0.8], [0.2, 0.8]]]);
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("backyard").unwrap().privacy_mask = mask_v1.clone();
    }

    // 1a. Without pre-seeded share_redacted_exp_by_masked.mp4, synthetic non-MP4 bytes fail closed (404)
    //     and NEVER leak the unredacted raw export file!
    let r_exp_fail_closed = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{exp_by_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_exp_fail_closed.status(), StatusCode::NOT_FOUND);

    // 1b. With redacted export + matching .mask_hash sidecar present, share_video_mp4
    //     serves share_redacted_exp_by_masked.mp4 instead of raw exp_by_masked.mp4
    let redacted_exp_file = exports_dir.join("share_redacted_exp_by_masked.mp4");
    let redacted_exp_sidecar = exports_dir.join(".share_redacted_exp_by_masked.mask_hash");
    std::fs::write(&redacted_exp_file, b"REDACTED_EXPORT_V1_BYTES").unwrap();
    std::fs::write(
        &redacted_exp_sidecar,
        compute_privacy_mask_hash(Some(&mask_v1)),
    )
    .unwrap();
    let r_exp_redacted_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{exp_by_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_exp_redacted_ok.status(), StatusCode::OK);
    let served_exp_bytes = to_bytes(r_exp_redacted_ok.into_body(), 4096).await.unwrap();
    assert_eq!(&served_exp_bytes[..], b"REDACTED_EXPORT_V1_BYTES");
    assert_ne!(&served_exp_bytes[..], &raw_export_bytes[..]);
    assert_eq!(
        std::fs::read_to_string(&redacted_exp_sidecar)
            .unwrap()
            .trim(),
        compute_privacy_mask_hash(Some(&mask_v1))
    );

    // 1c. Changing backyard privacy_mask to mask_v2 invalidates share_redacted_exp_by_masked.mp4 + sidecar
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("backyard").unwrap().privacy_mask = mask_v2.clone();
    }
    let r_exp_mask_v2 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{exp_by_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_exp_mask_v2.status(), StatusCode::NOT_FOUND);
    assert!(!redacted_exp_file.exists());
    assert!(!redacted_exp_sidecar.exists());

    // =========================================================================
    // 2. [ALTO-05 & MEDIO-08] Single-Segment share_clip_<id>.mp4 Creation & Recording Deletion Cache Invalidation
    // =========================================================================
    let seg_fd_single = rec_fd_dir.join("00.00.mp4");
    std::fs::write(&seg_fd_single, b"SINGLE_SEG_FD_BYTES").unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_fd_single_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/00.00.mp4".to_string(),
            start_time: 1000.0,
            end_time: 1010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_fd_single".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 1001.0,
            end_time: Some(1008.0),
            has_clip: true,
            has_snapshot: true,
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
    state
        .db
        .insert_review_segment(&ninjanvr_db::ReviewItemDto {
            id: "rev_fd_single".to_string(),
            camera: "front_door".to_string(),
            start_time: 1001.0,
            end_time: Some(1008.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-front_door-rev_fd_single.webp"
                .to_string(),
            data: json!({}),
            has_been_reviewed: false,
        })
        .unwrap();
    state
        .db
        .insert_share_link(
            "shr_ev_fd_single",
            "event",
            "ev_fd_single",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let ev_fd_single_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "event".to_string(),
            id: "ev_fd_single".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_ev_fd_single".to_string()),
        },
    );

    // 2a. Single-segment unmasked event builds exports/share_clip_ev_fd_single.mp4
    let r_single_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ev_fd_single_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_single_ok.status(), StatusCode::OK);
    let single_clip_path = exports_dir.join("share_clip_ev_fd_single.mp4");
    assert!(
        single_clip_path.is_file(),
        "single-segment shared event must build exports/share_clip_<id>.mp4"
    );

    // Seed additional cached share artifacts for ev_fd_single and rev_fd_single
    let ev_redacted_mp4 = exports_dir.join("share_redacted_ev_fd_single.mp4");
    let ev_redacted_hash = exports_dir.join(".share_redacted_ev_fd_single.mask_hash");
    let rev_clip_mp4 = exports_dir.join("share_clip_rev_fd_single.mp4");
    let rev_redacted_webp = clips_dir.join("share_redacted_rev_fd_single.webp");
    std::fs::write(&ev_redacted_mp4, b"CACHED_REDACTED").unwrap();
    std::fs::write(&ev_redacted_hash, b"HASH").unwrap();
    std::fs::write(&rev_clip_mp4, b"CACHED_REV_CLIP").unwrap();
    std::fs::write(&rev_redacted_webp, b"CACHED_WEBP").unwrap();

    // 2b. Deleting recordings via DELETE /api/front_door/recordings removes cached share_clip_* and share_redacted_* artifacts
    let r_del_recs = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/front_door/recordings?after=990&before=1020")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_del_recs.status(), StatusCode::OK);
    assert!(!single_clip_path.exists());
    assert!(!ev_redacted_mp4.exists());
    assert!(!ev_redacted_hash.exists());
    assert!(!rev_clip_mp4.exists());
    assert!(!rev_redacted_webp.exists());

    // 2c. Even if a stale share_clip_* or share_redacted_* file exists on disk when no underlying recording exists,
    //     share_video_mp4 verifies recording existence FIRST, deletes the stale file, and returns 404 Not Found!
    std::fs::write(&single_clip_path, b"STALE_CLIP_BYTES").unwrap();
    let r_stale_clip_404 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ev_fd_single_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_stale_clip_404.status(), StatusCode::NOT_FOUND);
    assert!(
        !single_clip_path.exists(),
        "stale share_clip_ must be removed when underlying recordings are missing"
    );

    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask = mask_v1.clone();
    }
    std::fs::write(&ev_redacted_mp4, b"STALE_REDACTED_BYTES").unwrap();
    std::fs::write(&ev_redacted_hash, compute_privacy_mask_hash(Some(&mask_v1))).unwrap();
    let r_stale_redacted_404 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ev_fd_single_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_stale_redacted_404.status(), StatusCode::NOT_FOUND);
    assert!(
        !ev_redacted_mp4.exists(),
        "stale share_redacted_ must be removed when underlying recordings are missing"
    );
    assert!(!ev_redacted_hash.exists());
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask = Value::Null;
    }

    // =========================================================================
    // 3. [MEDIO-07] Dot-Files in /exports/* and /clips/* Blocked Over HTTP (403 FORBIDDEN)
    // =========================================================================
    let dot_mask_hash = exports_dir.join(".share_redacted_ev_secret.mask_hash");
    let dot_tmp_mp4 = exports_dir.join(".share_clip_ev_secret.1234.tmp.mp4");
    let dot_clip_webp = clips_dir.join(".internal_sidecar.webp");
    std::fs::write(&dot_mask_hash, b"HASH_SECRET").unwrap();
    std::fs::write(&dot_tmp_mp4, b"TMP_SECRET").unwrap();
    std::fs::write(&dot_clip_webp, b"CLIP_SECRET").unwrap();

    for dot_uri in [
        "/exports/.share_redacted_ev_secret.mask_hash",
        "/exports/%2eshare_redacted_ev_secret.mask_hash",
        "/exports/%2Eshare_redacted_ev_secret.mask_hash",
        "/exports/.share_clip_ev_secret.1234.tmp.mp4",
        "/clips/.internal_sidecar.webp",
        "/clips/review/.hidden_thumb.webp",
    ] {
        assert_eq!(
            resolve_static_media_camera(&state, dot_uri),
            None,
            "resolve_static_media_camera must return None for {dot_uri}"
        );
        let r_admin = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(dot_uri)
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_admin.status(),
            StatusCode::FORBIDDEN,
            "admin must get 403 FORBIDDEN on dot-file {dot_uri}"
        );

        let r_op = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(dot_uri)
                    .header(header::AUTHORIZATION, &scoped_op_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_op.status(),
            StatusCode::FORBIDDEN,
            "scoped operator must get 403 FORBIDDEN on dot-file {dot_uri}"
        );

        let r_unauth_router = unauth_app
            .clone()
            .oneshot(Request::builder().uri(dot_uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            r_unauth_router.status(),
            StatusCode::FORBIDDEN,
            "unauth router must also block dot-file {dot_uri} with 403 FORBIDDEN"
        );
    }

    // =========================================================================
    // 4. [MEDIO-07] resolve_static_media_camera Fails Closed (None) for share_clip_ / share_redacted_ Absent from DB
    // =========================================================================
    for orphan_share_path in [
        "/exports/share_redacted_front_door_ghost_999.mp4",
        "/exports/share_clip_front_door_ghost_999.mp4",
        "/clips/share_redacted_front_door-ghost.webp",
        "/clips/share_clip_front_door-ghost.webp",
    ] {
        assert_eq!(
            resolve_static_media_camera(&state, orphan_share_path),
            None,
            "resolve_static_media_camera must fail closed (None) for absent DB target on {orphan_share_path}"
        );
        let r_scoped = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(orphan_share_path)
                    .header(header::AUTHORIZATION, &scoped_op_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_scoped.status(),
            StatusCode::FORBIDDEN,
            "camera-scoped operator must get 403 FORBIDDEN on orphan share artifact {orphan_share_path}"
        );
    }

    // =========================================================================
    // 5. [MEDIO-06] share_ffmpeg_semaphore Concurrency Gate, share_clip_locks Pruning (>= 64), & Credit Map Bounding (>= 512)
    // =========================================================================
    assert_eq!(state.share_engine.ffmpeg_semaphore.available_permits(), 2);

    // 5a. Hold both semaphore permits and verify a concurrent share_video_mp4 transcode waits for a permit
    std::fs::create_dir_all(&rec_fd_dir).unwrap();
    let seg_sem = rec_fd_dir.join("00.20.mp4");
    std::fs::write(&seg_sem, b"SEM_SEG_BYTES").unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_fd_sem_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-03/12/front_door/00.20.mp4".to_string(),
            start_time: 4000.0,
            end_time: 4010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
    // Long event (start=4000, end=99999) also exercises the 300s duration clamp (`end.max(start).min(start + 300.0)`)
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_fd_sem".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 4001.0,
            end_time: Some(99999.0),
            has_clip: true,
            has_snapshot: true,
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
    state
        .db
        .insert_share_link(
            "shr_ev_fd_sem",
            "event",
            "ev_fd_sem",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let ev_fd_sem_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "event".to_string(),
            id: "ev_fd_sem".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_ev_fd_sem".to_string()),
        },
    );

    let p1 = state
        .share_engine
        .ffmpeg_semaphore
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let p2 = state
        .share_engine
        .ffmpeg_semaphore
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    assert_eq!(state.share_engine.ffmpeg_semaphore.available_permits(), 0);

    let app_sem = auth_app.clone();
    let tok_sem = ev_fd_sem_tok.clone();
    let mut build_task = tokio::spawn(async move {
        app_sem
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{tok_sem}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    });

    // While both permits are held, build_task must remain pending (blocked on share_ffmpeg_semaphore)
    let early_poll = tokio::time::timeout(Duration::from_millis(40), &mut build_task).await;
    assert!(
        early_poll.is_err(),
        "share_video_mp4 build must wait when all share_ffmpeg_semaphore permits are held"
    );

    // Releasing one permit unblocks build_task to completion (200 OK)
    drop(p1);
    let sem_resp = build_task.await.unwrap();
    assert_eq!(sem_resp.status(), StatusCode::OK);
    drop(p2);
    assert_eq!(state.share_engine.ffmpeg_semaphore.available_permits(), 2);

    // 5b. share_clip_locks prunes idle entries (strong_count == 1) when map.len() >= 64 while preserving held locks
    {
        state.share_engine.clip_locks.lock().await.clear();
    }
    let held_active = acquire_share_clip_lock(&state, "active_held_lock").await;
    for i in 0..63 {
        let _idle = acquire_share_clip_lock(&state, &format!("idle_clip_lock_{i}")).await;
    }
    assert_eq!(state.share_engine.clip_locks.lock().await.len(), 64);

    // Acquiring a 65th lock triggers pruning of all 63 idle locks while retaining `active_held_lock`
    let new_lock = acquire_share_clip_lock(&state, "post_prune_lock").await;
    {
        let map = state.share_engine.clip_locks.lock().await;
        assert_eq!(
            map.len(),
            2,
            "all 63 idle locks must be pruned when map.len() >= 64, leaving only active_held_lock + post_prune_lock"
        );
        assert!(Arc::ptr_eq(
            map.get("active_held_lock").unwrap(),
            &held_active
        ));
        assert!(Arc::ptr_eq(map.get("post_prune_lock").unwrap(), &new_lock));
    }
    drop(held_active);
    drop(new_lock);

    // 5c. share_page_video_credits bounds map size at <= 512
    {
        let mut map = state.share_engine.page_video_credits.lock().unwrap();
        map.clear();
        for i in 0..512 {
            map.insert(
                format!("jti_credit_fill_{i}"),
                SharePlaybackCredit {
                    page_credits: if i % 2 == 0 { 0 } else { 1 },
                    active_stream_started: false,
                    min_next_range_start: u64::MAX,
                    ..Default::default()
                },
            );
        }
        assert_eq!(map.len(), 512);
    }
    let r_page_credit = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ev_fd_sem_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_page_credit.status(), StatusCode::OK);
    {
        let map = state.share_engine.page_video_credits.lock().unwrap();
        assert!(
            map.len() <= 512,
            "share_page_video_credits must be bounded to <= 512 entries (got {})",
            map.len()
        );
        assert!(map.contains_key("shr_ev_fd_sem"));
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_share_link_round2_audit_findings_regression_work() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr_sec_share_r2_{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
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
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let auth_app = build_router_with_auth(state.clone(), true);

    let now = unix_now();
    let make_jwt = |sub: &str, role: &str, allowed_cameras: Option<&str>| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: allowed_cameras.map(str::to_string),
                iat: unix_now(),
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };

    let admin_jwt = make_jwt("admin", "admin", Some("*"));
    let admin_bearer = format!("Bearer {admin_jwt}");

    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    let rec_dir = media_dir.join("recordings/2026-11-04/12/front_door");
    let rec_staging_dir = media_dir.join("recordings/.staging/front_door");
    std::fs::create_dir_all(&exports_dir).unwrap();
    std::fs::create_dir_all(&clips_dir).unwrap();
    std::fs::create_dir_all(&rec_dir).unwrap();
    std::fs::create_dir_all(&rec_staging_dir).unwrap();

    let mp4_payload: Vec<u8> = (0u8..128u8).collect();
    std::fs::write(exports_dir.join("exp_r2_fd.mp4"), &mp4_payload).unwrap();
    std::fs::write(exports_dir.join("exp_r2_by.mp4"), &mp4_payload).unwrap();
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_r2_fd".to_string(),
            camera: "front_door".to_string(),
            name: "R2 Front Door".to_string(),
            date: 1700000100.0,
            video_path: "/media/ninjanvr/exports/exp_r2_fd.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_r2_fd.webp".to_string(),
            in_progress: false,
        })
        .unwrap();
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_r2_by".to_string(),
            camera: "backyard".to_string(),
            name: "R2 Backyard".to_string(),
            date: 1700000100.0,
            video_path: "/media/ninjanvr/exports/exp_r2_by.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_r2_by.webp".to_string(),
            in_progress: false,
        })
        .unwrap();

    // =========================================================================
    // 1. [FINDING-01 & FINDING-02] Playback Session Grant (`sid`) + Range Unification (416)
    // =========================================================================
    let resp_create = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r2_fd","expires_in_sec":15,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp_create.status(), StatusCode::OK);
    let create_json: Value =
        serde_json::from_slice(&to_bytes(resp_create.into_body(), 4096).await.unwrap()).unwrap();
    let share_id = create_json["id"].as_str().unwrap().to_string();
    let share_tok = create_json["token"].as_str().unwrap().to_string();
    let exp_ts = create_json["expires_at"].as_u64().unwrap();
    // [FINDING-03 / FINDING-08] TTL < 60s (15s) is preserved without clamping up to 60s
    assert!(
        exp_ts <= unix_now() + 20,
        "expires_in_sec=15 must not be clamped to 60s (got delta {})",
        exp_ts.saturating_sub(unix_now())
    );

    // Load player page (consumes the 1 view and embeds ?sid=<32-hex> in <video> and <a download>)
    let page_resp = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{share_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page_resp.status(), StatusCode::OK);
    let page_html = String::from_utf8(
        to_bytes(page_resp.into_body(), 16384)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let sid_marker = format!("/share/{share_tok}/video.mp4?sid=");
    let sid_pos = page_html
        .find(&sid_marker)
        .expect("player HTML must embed ?sid= on video/download URLs");
    let sid_slice = &page_html[sid_pos + sid_marker.len()..];
    let sid: String = sid_slice
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    assert_eq!(sid.len(), 32);
    assert_eq!(
        state.db.get_share_link(&share_id).unwrap().unwrap()["access_count"],
        1
    );

    // Malformed Range: bytes=2-invalid returns 416 RANGE_NOT_SATISFIABLE without consuming session or views
    let bad_range_resp = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{share_tok}/video.mp4?sid={sid}"))
                .header(header::RANGE, "bytes=2-invalid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_range_resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);

    // Within the authenticated player session (`?sid={sid}`), Safari 0-1 probe, 0-63 read,
    // open-ended mid-stream Range: bytes=64-, backward seek Range: bytes=0-31, and Download MP4
    // (no Range header) ALL succeed on max_views=1 without incrementing access_count!
    for range_opt in [
        Some("bytes=0-1"),
        Some("bytes=0-63"),
        Some("bytes=64-"),
        Some("bytes=0-31"),
        None,
    ] {
        let mut req = Request::builder().uri(format!("/share/{share_tok}/video.mp4?sid={sid}"));
        if let Some(r_val) = range_opt {
            req = req.header(header::RANGE, r_val);
        }
        let r = auth_app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let expected_status = if range_opt.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        };
        assert_eq!(
            r.status(),
            expected_status,
            "valid sid session request with Range={range_opt:?} must succeed on max_views=1"
        );
        assert_eq!(
            state.db.get_share_link(&share_id).unwrap().unwrap()["access_count"],
            1
        );
    }

    // Second client without `sid` or with wrong `sid` gets 410 GONE on max_views=1
    for bad_uri in [
        format!("/share/{share_tok}/video.mp4"),
        format!("/share/{share_tok}/video.mp4?sid=00000000000000000000000000000000"),
        format!("/share/{share_tok}"),
    ] {
        let r_unauth = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&bad_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_unauth.status(),
            StatusCode::GONE,
            "uncredited viewer on {bad_uri} must get 410 GONE when max_views=1 is exhausted"
        );
    }

    // Direct API/video playback without `sid`: Safari 0-1 probe -> 0-31 -> open-ended 32- works on max_views=1
    let resp_direct = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r2_fd","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let direct_json: Value =
        serde_json::from_slice(&to_bytes(resp_direct.into_body(), 4096).await.unwrap()).unwrap();
    let direct_tok = direct_json["token"].as_str().unwrap().to_string();
    for range_hdr in ["bytes=0-1", "bytes=0-31", "bytes=32-", "bytes=64-"] {
        let r = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{direct_tok}/video.mp4"))
                    .header(header::RANGE, range_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::PARTIAL_CONTENT,
            "direct Range={range_hdr} sequence must succeed on max_views=1"
        );
    }
    // Concurrent/duplicate Range: bytes=64- at the already-consumed offset is rejected with 410 GONE
    let r_dup_offset = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{direct_tok}/video.mp4"))
                .header(header::RANGE, "bytes=64-")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_dup_offset.status(), StatusCode::GONE);

    // =========================================================================
    // 2. [FINDING-04] OIDC / External Creator (`ext:`) & Demoted Local `admin` DB User
    // =========================================================================
    {
        let mut cfg = state.config.write().await;
        cfg.auth.oidc.enabled = true;
        cfg.auth.oidc.issuer_url = "https://idp.corp.example".to_string();
    }
    let oidc_jwt = make_jwt("alice.sso@corp.example", "admin", Some("*"));
    let resp_oidc_create = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, format!("Bearer {oidc_jwt}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_r2_fd","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_oidc_create.status(), StatusCode::OK);
    let oidc_share_json: Value =
        serde_json::from_slice(&to_bytes(resp_oidc_create.into_body(), 4096).await.unwrap())
            .unwrap();
    let oidc_share_id = oidc_share_json["id"].as_str().unwrap().to_string();
    let oidc_share_tok = oidc_share_json["token"].as_str().unwrap().to_string();
    assert_eq!(
        state.db.get_share_link(&oidc_share_id).unwrap().unwrap()["created_by"],
        "ext:alice.sso@corp.example"
    );
    let r_oidc_play = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{oidc_share_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_oidc_play.status(), StatusCode::OK);
    {
        let mut cfg = state.config.write().await;
        cfg.auth.oidc.enabled = false;
        cfg.auth.oidc.issuer_url.clear();
    }

    // Demoting local `admin` user in SQLite to `operator` with `allowed_cameras = "front_door"`
    // blocks `admin`'s share link for `"backyard"` with 403 FORBIDDEN!
    let resp_admin_by = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_r2_by","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_admin_by.status(), StatusCode::OK);
    let admin_by_json: Value =
        serde_json::from_slice(&to_bytes(resp_admin_by.into_body(), 4096).await.unwrap()).unwrap();
    let admin_by_tok = admin_by_json["token"].as_str().unwrap().to_string();

    state
            .db
            .execute_batch(
                "UPDATE user SET role = 'operator', allowed_cameras = 'front_door' WHERE username = 'admin';",
            )
            .unwrap();
    let r_demoted_admin = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{admin_by_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_demoted_admin.status(),
        StatusCode::FORBIDDEN,
        "demoted local admin DB user must be blocked from sharing unauthorized camera"
    );
    state
        .db
        .execute_batch(
            "UPDATE user SET role = 'admin', allowed_cameras = '*' WHERE username = 'admin';",
        )
        .unwrap();

    // =========================================================================
    // 3. [FINDING-05] Percent-Encoded Static Media Paths & `/recordings/` Dot-Files Blocked
    // =========================================================================
    state
        .db
        .create_user_with_cameras("scoped_r2", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    let scoped_r2_bearer = format!(
        "Bearer {}",
        make_jwt("scoped_r2", "operator", Some("front_door"))
    );
    std::fs::write(
        exports_dir.join("share_clip_front_door_ghost_r2.mp4"),
        b"GHOST",
    )
    .unwrap();
    std::fs::write(rec_staging_dir.join("seg.mp4"), b"STAGING").unwrap();
    std::fs::write(rec_dir.join(".compact.mp4"), b"COMPACT").unwrap();

    for encoded_or_hidden in [
        "/exports/%73hare_clip_front_door_ghost_r2.mp4",
        "/exports/%73hare_redacted_front_door_ghost_r2.mp4",
        "/exports/share_clip_front_door_ghost_r2.mp%34",
        "/clips/%73hare_clip_front_door-ghost.webp",
        "/recordings/.staging/front_door/seg.mp4",
        "/recordings/%2estaging/front_door/seg.mp4",
        "/recordings/2026-11-04/12/front_door/.compact.mp4",
    ] {
        assert_eq!(
            resolve_static_media_camera(&state, encoded_or_hidden),
            None,
            "resolve_static_media_camera must return None on {encoded_or_hidden}"
        );
        let r = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(encoded_or_hidden)
                    .header(header::AUTHORIZATION, &scoped_r2_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::FORBIDDEN,
            "percent-encoded or dot-segment static path {encoded_or_hidden} must return 403 FORBIDDEN"
        );
    }

    // =========================================================================
    // 4. [FINDING-06] Pre-existing `share_redacted_<id>.mp4` WITHOUT `.mask_hash` Sidecar Is Invalidated
    // =========================================================================
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("backyard").unwrap().privacy_mask =
            json!([[[0.1, 0.1], [0.5, 0.1], [0.5, 0.5], [0.1, 0.5]]]);
    }
    let unverified_redacted = exports_dir.join("share_redacted_exp_r2_by.mp4");
    let unverified_sidecar = exports_dir.join(".share_redacted_exp_r2_by.mask_hash");
    std::fs::write(&unverified_redacted, b"STALE_UNVERIFIED_REDACTED_MP4").unwrap();
    let _ = std::fs::remove_file(&unverified_sidecar);

    let r_missing_sidecar = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{admin_by_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_missing_sidecar.status(),
        StatusCode::NOT_FOUND,
        "missing .mask_hash sidecar must invalidate pre-existing redacted MP4 and fail closed when rebuild fails"
    );
    assert!(
        !unverified_redacted.exists(),
        "unverified share_redacted_<id>.mp4 without .mask_hash must be removed"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_share_link_round4_audit_findings_regression_work() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr_sec_share_r4_{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
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
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let auth_app = build_router_with_auth(state.clone(), true);

    let now = unix_now();
    let make_jwt = |sub: &str, role: &str, allowed_cameras: Option<&str>| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: allowed_cameras.map(str::to_string),
                iat: unix_now(),
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };

    let admin_jwt = make_jwt("admin", "admin", Some("*"));
    let admin_bearer = format!("Bearer {admin_jwt}");
    let admin_cookie = format!("ninjanvr_token={admin_jwt}");

    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    std::fs::create_dir_all(&exports_dir).unwrap();
    std::fs::create_dir_all(&clips_dir).unwrap();

    let mp4_payload: Vec<u8> = (0u8..128u8).collect();
    std::fs::write(exports_dir.join("exp_r4_fd.mp4"), &mp4_payload).unwrap();
    std::fs::write(exports_dir.join("exp_r4_by.mp4"), &mp4_payload).unwrap();
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_r4_fd".to_string(),
            camera: "front_door".to_string(),
            name: "R4 Front Door".to_string(),
            date: 1700000100.0,
            video_path: "/media/ninjanvr/exports/exp_r4_fd.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_r4_fd.webp".to_string(),
            in_progress: false,
        })
        .unwrap();
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_r4_by".to_string(),
            camera: "backyard".to_string(),
            name: "R4 Backyard".to_string(),
            date: 1700000100.0,
            video_path: "/media/ninjanvr/exports/exp_r4_by.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_r4_by.webp".to_string(),
            in_progress: false,
        })
        .unwrap();

    // =========================================================================
    // 1. [FINDING-01] Multi-viewer `sessions` map, single `page_credits` claim per `sid`,
    //    64-session bound, and multi-cursor `active_streams`
    // =========================================================================
    let resp_mv2 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r4_fd","expires_in_sec":3600,"max_views":2}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(resp_mv2.status(), StatusCode::OK);
    let mv2_json: Value =
        serde_json::from_slice(&to_bytes(resp_mv2.into_body(), 4096).await.unwrap()).unwrap();
    let mv2_id = mv2_json["id"].as_str().unwrap().to_string();
    let mv2_tok = mv2_json["token"].as_str().unwrap().to_string();

    let extract_sid = |html: &str, tok: &str| -> String {
        let marker = format!("/share/{tok}/video.mp4?sid=");
        let pos = html.find(&marker).expect("player HTML must embed ?sid=");
        html[pos + marker.len()..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect()
    };

    // Viewer A loads player page (view 1) -> mints sid_a
    let page_a = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{mv2_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page_a.status(), StatusCode::OK);
    let html_a =
        String::from_utf8(to_bytes(page_a.into_body(), 16384).await.unwrap().to_vec()).unwrap();
    let sid_a = extract_sid(&html_a, &mv2_tok);
    assert_eq!(sid_a.len(), 32);

    // Viewer B loads player page (view 2) -> mints sid_b WITHOUT overwriting sid_a
    let page_b = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{mv2_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page_b.status(), StatusCode::OK);
    let html_b =
        String::from_utf8(to_bytes(page_b.into_body(), 16384).await.unwrap().to_vec()).unwrap();
    let sid_b = extract_sid(&html_b, &mv2_tok);
    assert_eq!(sid_b.len(), 32);
    assert_ne!(sid_a, sid_b);
    assert_eq!(
        state.db.get_share_link(&mv2_id).unwrap().unwrap()["access_count"],
        2
    );
    {
        let map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get(&mv2_id).unwrap();
        assert_eq!(entry.page_credits, 2);
        assert!(entry.sessions.contains_key(&sid_a));
        assert!(entry.sessions.contains_key(&sid_b));
    }

    // Viewer A issues 5 requests with sid_a (Safari probe, range chunks, seek, download):
    // all succeed on max_views=2 and only decrement page_credits ONCE (2 -> 1), leaving Viewer B's credit intact!
    for range_opt in [
        Some("bytes=0-1"),
        Some("bytes=0-31"),
        Some("bytes=32-63"),
        Some("bytes=0-15"),
        None,
    ] {
        let mut req = Request::builder().uri(format!("/share/{mv2_tok}/video.mp4?sid={sid_a}"));
        if let Some(r_val) = range_opt {
            req = req.header(header::RANGE, r_val);
        }
        let r = auth_app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let expected = if range_opt.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        };
        assert_eq!(r.status(), expected);
    }
    {
        let map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get(&mv2_id).unwrap();
        assert_eq!(
            entry.page_credits, 1,
            "Viewer A must not cross-drain Viewer B's page_credits across multiple sid_a requests"
        );
        assert_eq!(entry.sessions.get(&sid_a).map(|(_, c)| *c), Some(true));
        assert_eq!(entry.sessions.get(&sid_b).map(|(_, c)| *c), Some(false));
    }

    // Viewer B now streams and downloads with sid_b, and Viewer A can still seek with sid_a
    for (sid_val, range_opt) in [
        (&sid_b, Some("bytes=0-31")),
        (&sid_b, Some("bytes=32-")),
        (&sid_b, None),
        (&sid_a, Some("bytes=64-127")),
    ] {
        let mut req = Request::builder().uri(format!("/share/{mv2_tok}/video.mp4?sid={sid_val}"));
        if let Some(r_val) = range_opt {
            req = req.header(header::RANGE, r_val);
        }
        let r = auth_app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let expected = if range_opt.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        };
        assert_eq!(r.status(), expected);
    }
    assert_eq!(
        state.db.get_share_link(&mv2_id).unwrap().unwrap()["access_count"],
        2
    );

    // Concurrent direct video streams without sid on max_views=2 maintain independent range cursors
    let resp_streams = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r4_fd","expires_in_sec":3600,"max_views":2}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let streams_json: Value =
        serde_json::from_slice(&to_bytes(resp_streams.into_body(), 4096).await.unwrap()).unwrap();
    let streams_tok = streams_json["token"].as_str().unwrap().to_string();
    for range_hdr in [
        "bytes=0-15",  // Stream 1 initial (cursor -> 16)
        "bytes=0-15",  // Stream 2 initial (cursor -> 16)
        "bytes=16-63", // Stream 1 advances cursor 16 -> 64
        "bytes=16-31", // Stream 2 still matches its own cursor 16 -> 32!
        "bytes=32-63", // Stream 2 advances 32 -> 64
    ] {
        let r = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{streams_tok}/video.mp4"))
                    .header(header::RANGE, range_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    }

    // Verify per-JTI session map bounds at <= 64 entries (evicting expired then oldest expires_at)
    let mut credit_unit = SharePlaybackCredit::default();
    for i in 0..64 {
        credit_unit.insert_session(format!("sid_{i}"), 1000 + i as u64, 900);
    }
    assert_eq!(credit_unit.sessions.len(), 64);
    credit_unit.insert_session("sid_new".to_string(), 2000, 900);
    assert_eq!(credit_unit.sessions.len(), 64);
    assert!(
        !credit_unit.sessions.contains_key("sid_0"),
        "oldest session sid_0 must be evicted when sessions reaches 64"
    );
    assert!(credit_unit.sessions.contains_key("sid_new"));

    // =========================================================================
    // 2. [FINDING-02] Strict Cookie CSRF Guard (missing Origin/Referer, Sec-Fetch-Site: same-site, cross-port XFH)
    // =========================================================================
    // 2a. Cookie request with NO Origin and NO Referer (or Sec-Fetch-Site: none / same-site) is rejected (403)
    for sfs_opt in [None, Some("none"), Some("same-site"), Some("cross-site")] {
        let mut req_b = Request::builder()
            .method("POST")
            .uri("/api/share")
            .header(header::COOKIE, &admin_cookie)
            .header(header::HOST, "nvr.local:5000")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(sfs_val) = sfs_opt {
            req_b = req_b.header("sec-fetch-site", sfs_val);
        }
        let r = auth_app
            .clone()
            .oneshot(
                req_b
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r4_fd","expires_in_sec":3600}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::FORBIDDEN,
            "cookie POST /api/share without Origin/Referer and Sec-Fetch-Site={sfs_opt:?} must return 403"
        );
    }

    // 2b. Cookie request with Sec-Fetch-Site: same-site is rejected even when Origin/Host match
    let r_sfs_same_site = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::COOKIE, &admin_cookie)
                .header("sec-fetch-site", "same-site")
                .header(header::ORIGIN, "https://nvr.local:5000")
                .header(header::HOST, "nvr.local:5000")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_r4_fd","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_sfs_same_site.status(), StatusCode::FORBIDDEN);

    // 2c. Cross-port X-Forwarded-Host (`nvr.local:8971` vs `Host: nvr.local:5000`) is rejected (403)
    let r_xfh_cross_port = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::COOKIE, &admin_cookie)
                .header(header::ORIGIN, "https://nvr.local:8971")
                .header("x-forwarded-host", "nvr.local:8971")
                .header(header::HOST, "nvr.local:5000")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_r4_fd","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_xfh_cross_port.status(), StatusCode::FORBIDDEN);

    // 2d. Cookie request with no Origin/Referer (e.g. Referrer-Policy: no-referrer) + Sec-Fetch-Site: same-origin + Host succeeds (200 OK)
    let r_sfs_same_origin_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::COOKIE, &admin_cookie)
                .header("sec-fetch-site", "same-origin")
                .header(header::HOST, "nvr.local:5000")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_r4_fd","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_sfs_same_origin_ok.status(), StatusCode::OK);

    // =========================================================================
    // 3. [FINDING-03] Live SQLite Role/Camera Enforcement on Stale Admin JWTs, Deleted Admin JWTs, and `ext:` Creators
    // =========================================================================
    state
        .db
        .create_user_with_cameras("local_admin2", "pass1234", "admin", "*", 1000)
        .unwrap();
    let stale_admin2_jwt = make_jwt("local_admin2", "admin", Some("*"));
    let stale_admin2_bearer = format!("Bearer {stale_admin2_jwt}");

    // Demote local_admin2 in SQLite to operator restricted to front_door while JWT still claims role="admin"
    let r_demote_op = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/users/local_admin2/role")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"role":"operator","allowed_cameras":"front_door"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_demote_op.status(), StatusCode::OK);

    // Stale admin JWT is immediately blocked from sharing backyard (403) and admin-only /api/users (403)
    let r_stale_by = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &stale_admin2_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_r4_by","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_stale_by.status(), StatusCode::FORBIDDEN);

    let r_stale_users = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/users")
                .header(header::AUTHORIZATION, &stale_admin2_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_stale_users.status(), StatusCode::FORBIDDEN);

    // Demote local_admin2 to viewer -> stale admin JWT is blocked from POST /api/share even on front_door (403)
    state.db.update_user_role("local_admin2", "viewer").unwrap();
    state.auth_gate.invalidate_user("local_admin2");
    let r_stale_viewer = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &stale_admin2_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_r4_fd","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_stale_viewer.status(), StatusCode::FORBIDDEN);

    // Delete local_admin2 from SQLite -> stale admin JWT is rejected with 401 UNAUTHORIZED (never mints ext:local_admin2)
    state.db.delete_user("local_admin2").unwrap();
    state.auth_gate.invalidate_user("local_admin2");
    let r_deleted_admin2 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &stale_admin2_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_r4_fd","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_deleted_admin2.status(), StatusCode::UNAUTHORIZED);

    // Verify `ext:<user>` share link enforces DB user restrictions if a local user record exists for `<user>`
    state
        .db
        .create_user_with_cameras("sso_local_u", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    state
        .db
        .insert_share_link(
            "shr_ext_scoped_by",
            "export",
            "exp_r4_by",
            Some("backyard"),
            "ext:sso_local_u",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let ext_scoped_by_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_r4_by".to_string(),
            exp: now + 3600,
            cam: Some("backyard".to_string()),
            jti: Some("shr_ext_scoped_by".to_string()),
        },
    );
    let r_ext_by = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ext_scoped_by_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ext_by.status(), StatusCode::FORBIDDEN);

    // =========================================================================
    // 4. [FINDING-04] Missing/Pruned JTI Row Fails Closed in `record_share_access_or_gone` & `purge_revoked_share_credits`
    // =========================================================================
    let resp_prune_test = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r4_fd","expires_in_sec":3600,"max_views":2}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let prune_json: Value =
        serde_json::from_slice(&to_bytes(resp_prune_test.into_body(), 4096).await.unwrap())
            .unwrap();
    let prune_jti = prune_json["id"].as_str().unwrap().to_string();
    let prune_tok = prune_json["token"].as_str().unwrap().to_string();
    let prune_payload = verify_share_token(&state.jwt_secret, &prune_tok, now).unwrap();

    // Load player page to populate share_page_video_credits for prune_jti
    let prune_page = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{prune_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(prune_page.status(), StatusCode::OK);
    let prune_html = String::from_utf8(
        to_bytes(prune_page.into_body(), 16384)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let prune_sid = extract_sid(&prune_html, &prune_tok);
    assert!(
        state
            .share_engine
            .page_video_credits
            .lock()
            .unwrap()
            .contains_key(&prune_jti)
    );

    // Delete the share_link row from SQLite (simulating prune_expired_share_links)
    state
        .db
        .execute_batch(&format!("DELETE FROM share_link WHERE id = '{prune_jti}';"))
        .unwrap();

    // Direct call to record_share_access_or_gone with has_valid_sid=true must fail closed with 410 GONE and evict credit
    let gone_resp = record_share_access_or_gone(
        &state,
        &prune_payload,
        false,
        false,
        Some((0, Some(15))),
        now,
        Some(prune_sid),
        true,
        false,
        None,
        StatusCode::PARTIAL_CONTENT.into_response(),
    );
    assert_eq!(gone_resp.status(), StatusCode::GONE);
    assert!(
        !state
            .share_engine
            .page_video_credits
            .lock()
            .unwrap()
            .contains_key(&prune_jti)
    );

    // Also verify verify_post_build_share_target_and_jti fails closed with 410 GONE on missing row
    let post_build_gone =
        verify_post_build_share_target_and_jti(&state, &prune_payload, true, None, false)
            .expect("missing JTI row must fail closed in verify_post_build_share_target_and_jti");
    assert_eq!(post_build_gone.status(), StatusCode::GONE);

    // =========================================================================
    // 5. [FINDING-05] Camera Scoping on `POST /api/reviews/delete` & Path Containment in `api_export_delete`
    // =========================================================================
    state
        .db
        .create_user_with_cameras("scoped_r4_fd", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    let scoped_r4_fd_bearer = format!(
        "Bearer {}",
        make_jwt("scoped_r4_fd", "operator", Some("front_door"))
    );

    state
        .db
        .insert_review_segment(&ninjanvr_db::ReviewItemDto {
            id: "rev_r4_by".to_string(),
            camera: "backyard".to_string(),
            start_time: 1000.0,
            end_time: Some(1008.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-backyard-rev_r4_by.webp".to_string(),
            data: json!({}),
            has_been_reviewed: false,
        })
        .unwrap();
    state
        .db
        .insert_share_link(
            "shr_rev_r4_by",
            "review",
            "rev_r4_by",
            Some("backyard"),
            "admin",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();

    // Scoped operator restricted to front_door cannot delete backyard review via POST /api/reviews/delete
    let r_cross_rev_del = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/reviews/delete")
                .header(header::AUTHORIZATION, &scoped_r4_fd_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ids":["rev_r4_by"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_cross_rev_del.status(), StatusCode::FORBIDDEN);
    assert!(
        state.db.get_review_by_id("rev_r4_by").unwrap().is_some(),
        "unauthorized POST /api/reviews/delete must not delete backyard review"
    );
    assert!(
        !state.db.is_share_link_revoked("shr_rev_r4_by").unwrap(),
        "unauthorized POST /api/reviews/delete must not revoke backyard share link"
    );

    // Path containment in api_export_delete: traversal video_path/thumb_path must not unlink files outside media_dir
    let outside_keep = tmp_dir.join("outside_keep.mp4");
    std::fs::write(&outside_keep, b"DO_NOT_DELETE_OUTSIDE_MEDIA_DIR").unwrap();
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_r4_unlink_trav".to_string(),
            camera: "front_door".to_string(),
            name: "Traversal Unlink Test".to_string(),
            date: 1700000100.0,
            video_path: "/media/ninjanvr/exports/../../outside_keep.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/../../outside_keep.mp4".to_string(),
            in_progress: false,
        })
        .unwrap();
    let r_del_trav_exp = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/export/exp_r4_unlink_trav")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_del_trav_exp.status(), StatusCode::OK);
    assert!(
        outside_keep.is_file(),
        "DELETE /api/export must never unlink files outside media_dir"
    );

    // =========================================================================
    // 6. [FINDING-06] Export `video_path` Containment in `resolve_share_target_metadata`
    //    & Unmasked Export Share Audio Stripping
    // =========================================================================
    // 6a. Export row in DB with traversal path or missing file or in_progress=true fails closed (404)
    //     even if a same-named file exists in exports/
    std::fs::write(exports_dir.join("exp_r4_inprog.mp4"), &mp4_payload).unwrap();
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_r4_inprog".to_string(),
            camera: "front_door".to_string(),
            name: "In Progress Export".to_string(),
            date: 1700000100.0,
            video_path: "/media/ninjanvr/exports/exp_r4_inprog.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_r4_inprog.webp".to_string(),
            in_progress: true,
        })
        .unwrap();
    assert!(!resolve_share_target_metadata(&state, "export", "exp_r4_inprog").3);

    state
        .db
        .insert_export(&ExportRow {
            id: "exp_r4_trav_meta".to_string(),
            camera: "front_door".to_string(),
            name: "Traversal Meta Export".to_string(),
            date: 1700000100.0,
            video_path: "/media/ninjanvr/exports/../../outside_keep.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_r4_trav_meta.webp".to_string(),
            in_progress: false,
        })
        .unwrap();
    assert!(!resolve_share_target_metadata(&state, "export", "exp_r4_trav_meta").3);
    let r_share_trav_meta = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_r4_trav_meta","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_share_trav_meta.status(), StatusCode::NOT_FOUND);

    // 6b. Unmasked export MP4 containing an audio (`soun`) track never serves raw audio:
    //     builds/serves audio-free `exports/share_clip_{id}.mp4` or fails closed with 404 NOT_FOUND
    let build_mp4_with_audio = || -> Vec<u8> {
        let ftyp = make_mp4_box(b"ftyp", b"isom\x00\x00\x02\x00isom");
        let mdat = make_mp4_box(b"mdat", &[0x11u8; 32]);
        let mut hdlr_body = vec![0u8; 20];
        hdlr_body[4..8].copy_from_slice(b"soun");
        let hdlr = make_mp4_full_box(b"hdlr", 0, 0, &hdlr_body);
        let mdia = make_mp4_box(b"mdia", &hdlr);
        let soun_trak = make_mp4_box(b"trak", &mdia);
        let moov = make_mp4_box(b"moov", &soun_trak);
        let mut out = Vec::new();
        out.extend_from_slice(&ftyp);
        out.extend_from_slice(&mdat);
        out.extend_from_slice(&moov);
        out
    };
    let audio_mp4_bytes = build_mp4_with_audio();
    let audio_exp_path = exports_dir.join("exp_r4_with_audio.mp4");
    std::fs::write(&audio_exp_path, &audio_mp4_bytes).unwrap();
    assert!(ninjanvr_media::mp4_file_has_audio_track(&audio_exp_path));

    state
        .db
        .insert_export(&ExportRow {
            id: "exp_r4_with_audio".to_string(),
            camera: "front_door".to_string(),
            name: "Export With Audio".to_string(),
            date: 1700000100.0,
            video_path: "/media/ninjanvr/exports/exp_r4_with_audio.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_r4_with_audio.webp".to_string(),
            in_progress: false,
        })
        .unwrap();
    let resp_share_audio = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_r4_with_audio","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_share_audio.status(), StatusCode::OK);
    let share_audio_json: Value =
        serde_json::from_slice(&to_bytes(resp_share_audio.into_body(), 4096).await.unwrap())
            .unwrap();
    let share_audio_tok = share_audio_json["token"].as_str().unwrap().to_string();

    // When FFmpeg cannot strip synthetic non-bitstream bytes, share_video_mp4 fails closed (404) instead of serving audio_exp_path
    let r_audio_fail_closed = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{share_audio_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_audio_fail_closed.status(),
        StatusCode::NOT_FOUND,
        "unmasked export with audio track must fail closed (404) when audio strip fails, never serving raw audio"
    );

    // When audio-stripped share_clip_exp_r4_with_audio.mp4 (without audio track) is present, it is served (200 OK)
    let stripped_clip_path = exports_dir.join("share_clip_exp_r4_with_audio.mp4");
    let silent_mp4 = build_synthetic_mp4_bytes();
    std::fs::write(&stripped_clip_path, &silent_mp4).unwrap();
    assert!(!ninjanvr_media::mp4_file_has_audio_track(
        &stripped_clip_path
    ));

    let r_audio_stripped_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{share_audio_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_audio_stripped_ok.status(), StatusCode::OK);
    let served_stripped = to_bytes(r_audio_stripped_ok.into_body(), 8192)
        .await
        .unwrap();
    assert_eq!(&served_stripped[..], &silent_mp4[..]);
    assert_ne!(&served_stripped[..], &audio_mp4_bytes[..]);

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_share_link_round6_audit_findings_regression_work() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr_sec_share_r6_{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
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
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let auth_app = build_router_with_auth(state.clone(), true);

    let now = unix_now();
    let make_jwt = |sub: &str, role: &str, allowed_cameras: Option<&str>| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: allowed_cameras.map(str::to_string),
                iat: unix_now(),
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };

    let admin_jwt = make_jwt("admin", "admin", Some("*"));
    let admin_bearer = format!("Bearer {admin_jwt}");
    let admin_cookie = format!("ninjanvr_token={admin_jwt}");

    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    let rec_dir = media_dir.join("recordings/2026-11-04/12/front_door");
    std::fs::create_dir_all(&exports_dir).unwrap();
    std::fs::create_dir_all(&clips_dir).unwrap();
    std::fs::create_dir_all(&rec_dir).unwrap();

    let mp4_payload: Vec<u8> = (0u8..128u8).collect();
    for exp_id in [
        "exp_r6_tok",
        "exp_r6_cam",
        "exp_r6_inflight",
        "exp_r6_u_del",
        "exp_r6_u_role",
        "exp_r6_u_cam",
        "exp_r6_tok_del",
        "exp_r6_416",
        "exp_r6_csrf",
    ] {
        std::fs::write(exports_dir.join(format!("{exp_id}.mp4")), &mp4_payload).unwrap();
        state
            .db
            .insert_export(&ExportRow {
                id: exp_id.to_string(),
                camera: "front_door".to_string(),
                name: format!("R6 {exp_id}"),
                date: 1700000100.0,
                video_path: format!("/media/ninjanvr/exports/{exp_id}.mp4"),
                thumb_path: format!("/media/ninjanvr/clips/{exp_id}.webp"),
                in_progress: false,
            })
            .unwrap();
    }

    let seed_artifacts = |target_id: &str| {
        let r_mp4 = exports_dir.join(format!("share_redacted_{target_id}.mp4"));
        let r_hash = exports_dir.join(format!(".share_redacted_{target_id}.mask_hash"));
        let c_mp4 = exports_dir.join(format!("share_clip_{target_id}.mp4"));
        std::fs::write(&r_mp4, b"REDACTED_ARTIFACT").unwrap();
        std::fs::write(&r_hash, b"MASK_HASH").unwrap();
        std::fs::write(&c_mp4, b"CLIP_ARTIFACT").unwrap();
        (r_mp4, r_hash, c_mp4)
    };

    // =========================================================================
    // 1. [R6-FINDING-01 & R6-FINDING-02] Post-Build Creator Token Expiry & Camera
    //    Revocation Auto-Revoke JTI in DB, Return 403 FORBIDDEN (Not 410 GONE),
    //    Clean Up Newly Built Artifacts, & Preserve In-Flight Playback Sessions
    // =========================================================================
    // 1a. Post-build creator API token expiry: auto-revokes JTI in DB, returns 403 FORBIDDEN,
    //     and removes newly built share_redacted_* / share_clip_* artifacts from disk!
    state
        .db
        .create_api_token("r6_expiring_bot", "full", Some((now - 5) as f64))
        .unwrap();
    state
        .db
        .insert_share_link(
            "shr_r6_post_tok",
            "export",
            "exp_r6_tok",
            Some("front_door"),
            "token:r6_expiring_bot",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    assert!(!state.db.is_share_link_revoked("shr_r6_post_tok").unwrap());
    let (r_mp4_tok, r_hash_tok, c_mp4_tok) = seed_artifacts("exp_r6_tok");
    let tok_payload = ShareTokenPayload {
        k: "export".to_string(),
        id: "exp_r6_tok".to_string(),
        exp: now + 3600,
        cam: Some("front_door".to_string()),
        jti: Some("shr_r6_post_tok".to_string()),
    };
    let post_tok_resp =
        verify_post_build_share_target_and_jti(&state, &tok_payload, true, None, false)
            .expect("expired creator API token must fail post-build check");
    assert_eq!(
        post_tok_resp.status(),
        StatusCode::FORBIDDEN,
        "expired creator API token must return 403 FORBIDDEN, not 410 GONE"
    );
    assert!(
        state.db.is_share_link_revoked("shr_r6_post_tok").unwrap(),
        "post-build creator token expiry must auto-revoke JTI in SQLite"
    );
    assert!(
        !r_mp4_tok.exists() && !r_hash_tok.exists() && !c_mp4_tok.exists(),
        "post-build creator token expiry must clean up share_redacted_* and share_clip_* artifacts"
    );

    // 1b. Post-build creator camera access revocation (via update_user_allowed_cameras, which sets revoked=1):
    //     returns 403 FORBIDDEN (NOT masked as 410 GONE!) and cleans up artifacts on disk!
    state
        .db
        .create_user_with_cameras(
            "r6_cam_op",
            "pass1234",
            "operator",
            "front_door,backyard",
            1000,
        )
        .unwrap();
    state
        .db
        .insert_share_link(
            "shr_r6_post_cam",
            "export",
            "exp_r6_cam",
            Some("front_door"),
            "r6_cam_op",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    state
        .db
        .update_user_allowed_cameras("r6_cam_op", "backyard")
        .unwrap();
    state.auth_gate.invalidate_user("r6_cam_op");
    assert!(state.db.is_share_link_revoked("shr_r6_post_cam").unwrap());
    let (r_mp4_cam, r_hash_cam, c_mp4_cam) = seed_artifacts("exp_r6_cam");
    let cam_payload = ShareTokenPayload {
        k: "export".to_string(),
        id: "exp_r6_cam".to_string(),
        exp: now + 3600,
        cam: Some("front_door".to_string()),
        jti: Some("shr_r6_post_cam".to_string()),
    };
    let post_cam_resp =
        verify_post_build_share_target_and_jti(&state, &cam_payload, true, None, false)
            .expect("revoked camera permission must fail post-build check");
    assert_eq!(
        post_cam_resp.status(),
        StatusCode::FORBIDDEN,
        "creator camera revocation in verify_post_build_share_target_and_jti must return 403 FORBIDDEN (not 410 GONE)"
    );
    assert!(
        !r_mp4_cam.exists() && !r_hash_cam.exists() && !c_mp4_cam.exists(),
        "post-build camera revocation must clean up share_redacted_* and share_clip_* artifacts"
    );

    // 1c. Preserve in-flight max_views=1 playback session artifacts when sibling link is revoked,
    //     then clean up artifacts once the in-flight session finishes!
    let resp_mv1 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r6_inflight","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let mv1_json: Value =
        serde_json::from_slice(&to_bytes(resp_mv1.into_body(), 4096).await.unwrap()).unwrap();
    let mv1_jti = mv1_json["id"].as_str().unwrap().to_string();
    let mv1_tok = mv1_json["token"].as_str().unwrap().to_string();

    let resp_sib = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r6_inflight","expires_in_sec":3600,"max_views":5}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let sib_json: Value =
        serde_json::from_slice(&to_bytes(resp_sib.into_body(), 4096).await.unwrap()).unwrap();
    let sib_jti = sib_json["id"].as_str().unwrap().to_string();

    // Viewer opens player page on mv1_tok -> access_count reaches 1 == max_views, session credit is active
    let page_mv1 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{mv1_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page_mv1.status(), StatusCode::OK);
    assert!(
        state
            .share_engine
            .page_video_credits
            .lock()
            .unwrap()
            .contains_key(&mv1_jti)
    );
    let (r_mp4_inf, r_hash_inf, c_mp4_inf) = seed_artifacts("exp_r6_inflight");

    // Revoking sibling link `sib_jti` while `mv1_jti` has an active in-flight session preserves the artifacts!
    let rev_sib = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/share/{sib_jti}"))
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rev_sib.status(), StatusCode::OK);
    assert!(
        r_mp4_inf.exists() && r_hash_inf.exists() && c_mp4_inf.exists(),
        "in-flight max_views=1 viewer session must preserve shared clip artifacts when sibling link is revoked"
    );

    // Once the in-flight session expires/clears, purge_revoked_share_credits cleans up the exhausted target's artifacts!
    state
        .share_engine
        .page_video_credits
        .lock()
        .unwrap()
        .clear();
    assert!(!has_active_or_in_flight_share_for_target(
        &state,
        "exp_r6_inflight",
        unix_now()
    ));
    purge_revoked_share_credits(&state);
    assert!(
        !r_mp4_inf.exists() && !r_hash_inf.exists() && !c_mp4_inf.exists(),
        "exhausted max_views=1 target without in-flight session must have artifacts cleaned up"
    );

    // =========================================================================
    // 2. [R6-FINDING-06a] Creator Deletion, Role Demotion, Camera Narrowing, &
    //    Token Deletion Clean Up Orphaned Share Artifacts on Disk
    // =========================================================================
    for (u_name, exp_id) in [
        ("r6_u_del", "exp_r6_u_del"),
        ("r6_u_role", "exp_r6_u_role"),
        ("r6_u_cam", "exp_r6_u_cam"),
    ] {
        state
            .db
            .create_user_with_cameras(u_name, "pass1234", "operator", "front_door,backyard", 1000)
            .unwrap();
        state
            .db
            .insert_share_link(
                &format!("shr_{u_name}"),
                "export",
                exp_id,
                Some("front_door"),
                u_name,
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
    }
    let (tok_meta, _) = state
        .db
        .create_api_token("r6_tok_del", "full", Some((now + 3600) as f64))
        .unwrap();
    let tok_del_id = tok_meta["id"].as_str().unwrap().to_string();
    state
        .db
        .insert_share_link(
            "shr_r6_tok_del",
            "export",
            "exp_r6_tok_del",
            Some("front_door"),
            "token:r6_tok_del",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();

    // 2a. DELETE /api/users/r6_u_del cleans up exp_r6_u_del artifacts
    let (r_del_mp4, r_del_hash, c_del_mp4) = seed_artifacts("exp_r6_u_del");
    let resp_u_del = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/users/r6_u_del")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_u_del.status(), StatusCode::OK);
    assert!(!r_del_mp4.exists() && !r_del_hash.exists() && !c_del_mp4.exists());

    // 2b. PUT /api/users/r6_u_role/role (viewer) cleans up exp_r6_u_role artifacts
    let (r_role_mp4, r_role_hash, c_role_mp4) = seed_artifacts("exp_r6_u_role");
    let resp_u_role = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/users/r6_u_role/role")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"role":"viewer"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_u_role.status(), StatusCode::OK);
    assert!(!r_role_mp4.exists() && !r_role_hash.exists() && !c_role_mp4.exists());

    // 2c. PUT /api/users/r6_u_cam/cameras (backyard) cleans up exp_r6_u_cam artifacts
    let (r_cam_mp4, r_cam_hash, c_cam_mp4) = seed_artifacts("exp_r6_u_cam");
    let resp_u_cam = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/users/r6_u_cam/cameras")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"allowed_cameras":"backyard"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_u_cam.status(), StatusCode::OK);
    assert!(!r_cam_mp4.exists() && !r_cam_hash.exists() && !c_cam_mp4.exists());

    // 2d. DELETE /api/tokens/{id} cleans up exp_r6_tok_del artifacts
    let (r_td_mp4, r_td_hash, c_td_mp4) = seed_artifacts("exp_r6_tok_del");
    let resp_tok_del = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/tokens/{tok_del_id}"))
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_tok_del.status(), StatusCode::OK);
    assert!(!r_td_mp4.exists() && !r_td_hash.exists() && !c_td_mp4.exists());

    // =========================================================================
    // 3. [R6-FINDING-06b] Out-of-Bounds Range Request (416 RANGE_NOT_SATISFIABLE)
    //    Rolls Back `active_streams[idx]` So Subsequent Valid Range Chunk Succeeds
    // =========================================================================
    let resp_416_share = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r6_416","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let share_416_json: Value =
        serde_json::from_slice(&to_bytes(resp_416_share.into_body(), 4096).await.unwrap()).unwrap();
    let share_416_tok = share_416_json["token"].as_str().unwrap().to_string();

    // Initial chunk Range: bytes=0-15 sets stream cursor to 16
    let r_chunk1 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{share_416_tok}/video.mp4"))
                .header(header::RANGE, "bytes=0-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_chunk1.status(), StatusCode::PARTIAL_CONTENT);

    // Out-of-bounds Range: bytes=999999- returns 416 RANGE_NOT_SATISFIABLE and rolls back cursor to 16
    let r_oob_416 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{share_416_tok}/video.mp4"))
                .header(header::RANGE, "bytes=999999-")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_oob_416.status(), StatusCode::RANGE_NOT_SATISFIABLE);

    // Next valid chunk Range: bytes=16-63 still succeeds with 206 PARTIAL_CONTENT!
    let r_chunk2 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{share_416_tok}/video.mp4"))
                .header(header::RANGE, "bytes=16-63")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_chunk2.status(),
        StatusCode::PARTIAL_CONTENT,
        "valid continuation range after 416 out-of-bounds range probe must still succeed"
    );

    // =========================================================================
    // 4. [R6-FINDING-03a] `is_internal_backend_host` Rejects RFC1918 LAN IPs While
    //    Accepting Loopback & Internal Service-Mesh DNS Names
    // =========================================================================
    for loopback_or_mesh in [
        "127.0.0.1",
        "127.0.0.1:5000",
        "127.0.0.2:8971",
        "[::1]",
        "[::1]:5000",
        "[0:0:0:0:0:0:0:1]:5000",
        "[::ffff:127.0.0.1]:5000",
        "localhost:5000",
        "ninjanvr:5000",
        "ninjanvr.default.svc",
        "ninjanvr.default.svc.cluster.local:5000",
        "gateway.internal:8443",
    ] {
        assert!(
            is_internal_backend_host(loopback_or_mesh),
            "{loopback_or_mesh} must be accepted as internal backend host"
        );
    }
    for lan_or_external in [
        "192.168.1.50",
        "192.168.1.50:5000",
        "10.0.0.50:5000",
        "172.16.0.10:5000",
        "169.254.1.10:5000",
        "[::ffff:192.168.1.50]:5000",
        "nvr.corp.example:5000",
    ] {
        assert!(
            !is_internal_backend_host(lan_or_external),
            "{lan_or_external} must NOT be trusted as internal backend host for X-Forwarded-Host"
        );
    }

    // Forged X-Forwarded-Host against LAN IP Host (`192.168.1.50:5000`) is rejected with 403 FORBIDDEN
    let r_lan_xfh_spoof = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::COOKIE, &admin_cookie)
                .header(header::ORIGIN, "https://evil.example")
                .header("x-forwarded-host", "evil.example")
                .header(header::HOST, "192.168.1.50:5000")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_r6_csrf","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_lan_xfh_spoof.status(),
        StatusCode::FORBIDDEN,
        "forged X-Forwarded-Host against RFC1918 LAN Host 192.168.1.50:5000 must return 403 FORBIDDEN"
    );

    // =========================================================================
    // 5. [R6-FINDING-03b] Cookie CSRF Rejection (403 FORBIDDEN) on All
    //    Target-Deleting & Share-Revoking Endpoints When Origin Mismatches Host
    // =========================================================================
    state
        .db
        .create_user_with_cameras("r6_csrf_target_u", "pass1234", "operator", "*", 1000)
        .unwrap();
    let (csrf_tok_meta, _) = state
        .db
        .create_api_token("r6_csrf_tok", "read_only", None)
        .unwrap();
    let csrf_tok_id = csrf_tok_meta["id"].as_str().unwrap().to_string();

    for (method, uri, body_opt) in [
        (
            "POST",
            "/api/reviews/delete".to_string(),
            Some(r#"{"ids":["rev_1"]}"#),
        ),
        (
            "DELETE",
            "/api/reviews".to_string(),
            Some(r#"{"ids":["rev_1"]}"#),
        ),
        ("DELETE", "/api/review/rev_1".to_string(), None),
        (
            "POST",
            "/api/exports/delete".to_string(),
            Some(r#"{"ids":["exp_r6_csrf"]}"#),
        ),
        ("DELETE", "/api/export/exp_r6_csrf".to_string(), None),
        ("DELETE", "/api/events/ev_1".to_string(), None),
        (
            "DELETE",
            "/api/events".to_string(),
            Some(r#"{"event_ids":["ev_1"]}"#),
        ),
        (
            "PUT",
            "/api/users/r6_csrf_target_u/role".to_string(),
            Some(r#"{"role":"viewer"}"#),
        ),
        (
            "PUT",
            "/api/users/r6_csrf_target_u/cameras".to_string(),
            Some(r#"{"allowed_cameras":"front_door"}"#),
        ),
        ("DELETE", "/api/users/r6_csrf_target_u".to_string(), None),
        ("DELETE", format!("/api/tokens/{csrf_tok_id}"), None),
        (
            "DELETE",
            "/api/front_door/recordings?after=0&before=9999".to_string(),
            None,
        ),
    ] {
        let mut req_b = Request::builder()
            .method(method)
            .uri(&uri)
            .header(header::COOKIE, &admin_cookie)
            .header(header::ORIGIN, "https://evil.example")
            .header(header::HOST, "192.168.1.50:5000");
        let body = if let Some(json_str) = body_opt {
            req_b = req_b.header(header::CONTENT_TYPE, "application/json");
            Body::from(json_str)
        } else {
            Body::empty()
        };
        let resp = auth_app
            .clone()
            .oneshot(req_b.body(body).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "cookie CSRF request {method} {uri} with mismatched Origin must return 403 FORBIDDEN"
        );
    }

    // Verify none of the protected resources were modified/deleted by the rejected CSRF requests
    assert!(state.db.get_export_by_id("exp_r6_csrf").unwrap().is_some());
    assert!(state.db.get_user("r6_csrf_target_u").unwrap().is_some());

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_share_link_round8_audit_findings_regression_work() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr_sec_share_r8_{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
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
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let auth_app = build_router_with_auth(state.clone(), true);

    let now = unix_now();
    let make_jwt = |sub: &str, role: &str, allowed_cameras: Option<&str>| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: allowed_cameras.map(str::to_string),
                iat: unix_now(),
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };

    let admin_jwt = make_jwt("admin", "admin", Some("*"));
    let admin_bearer = format!("Bearer {admin_jwt}");
    let scoped_fd_jwt = make_jwt("scoped_r8_fd", "operator", Some("front_door"));
    let scoped_fd_bearer = format!("Bearer {scoped_fd_jwt}");

    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    let rec_dir = media_dir.join("recordings/2026-11-04/12/front_door");
    std::fs::create_dir_all(&exports_dir).unwrap();
    std::fs::create_dir_all(&clips_dir).unwrap();
    std::fs::create_dir_all(&rec_dir).unwrap();

    let mp4_payload: Vec<u8> = (0u8..128u8).collect();
    for exp_id in [
        "exp_r8_ttl",
        "exp_r8_ds1",
        "exp_r8_ds2",
        "exp_r8_stream",
        "exp_r8_head",
        "exp_r8_head_rng",
        "exp_r8_c_role",
        "exp_r8_c_del",
        "exp_r8_c_ext",
        "exp_r8_mv_clean",
        "exp_r8_exp_clean",
    ] {
        std::fs::write(exports_dir.join(format!("{exp_id}.mp4")), &mp4_payload).unwrap();
        state
            .db
            .insert_export(&ExportRow {
                id: exp_id.to_string(),
                camera: "front_door".to_string(),
                name: format!("R8 {exp_id}"),
                date: 1700000100.0,
                video_path: format!("/media/ninjanvr/exports/{exp_id}.mp4"),
                thumb_path: format!("/media/ninjanvr/clips/{exp_id}.webp"),
                in_progress: false,
            })
            .unwrap();
    }

    let seed_artifacts = |target_id: &str| {
        let r_mp4 = exports_dir.join(format!("share_redacted_{target_id}.mp4"));
        let r_hash = exports_dir.join(format!(".share_redacted_{target_id}.mask_hash"));
        let c_mp4 = exports_dir.join(format!("share_clip_{target_id}.mp4"));
        std::fs::write(&r_mp4, b"R8_REDACTED").unwrap();
        std::fs::write(&r_hash, b"R8_HASH").unwrap();
        std::fs::write(&c_mp4, b"R8_CLIP").unwrap();
        (r_mp4, r_hash, c_mp4)
    };

    let extract_sid = |html: &str, tok: &str| -> String {
        let marker = format!("/share/{tok}/video.mp4?sid=");
        let pos = html.find(&marker).expect("player HTML must embed ?sid=");
        html[pos + marker.len()..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect()
    };

    // =========================================================================
    // 1. [FINDING-01] Unclaimed `page_credits` & `active_streams` TTL Expiry,
    //    Double-Spend Prevention, and Non-Destructive Range Continuation
    // =========================================================================
    // 1a. Unit tests on SharePlaybackCredit: unclaimed session expiry & eviction decrement page_credits,
    //     and active_stream_expires prunes expired stream cursors
    let mut unit_credit = SharePlaybackCredit {
        page_credits: 2,
        ..Default::default()
    };
    unit_credit.insert_session("s_unclaimed".to_string(), 1000, 900);
    unit_credit.insert_session("s_claimed".to_string(), 1000, 900);
    unit_credit.sessions.get_mut("s_claimed").unwrap().1 = true;
    unit_credit.record_initial_stream_at(64, 1000);
    assert!(unit_credit.has_any_active_stream());
    assert_eq!(unit_credit.active_stream_expires, vec![1000]);

    // At now_ts = 1000, s_unclaimed (unclaimed) and s_claimed (claimed) both expire -> page_credits decrements by 1 (not 2!),
    // and the expired stream cursor at 1000 is pruned!
    unit_credit.prune_expired_sessions(1000);
    assert_eq!(
        unit_credit.page_credits, 1,
        "only expired unclaimed sessions must decrement page_credits"
    );
    assert!(unit_credit.sessions.is_empty());
    assert!(!unit_credit.has_any_active_stream());
    assert!(unit_credit.active_streams.is_empty());
    assert!(unit_credit.active_stream_expires.is_empty());

    // 64-session eviction decrements page_credits when evicting an unclaimed session
    let mut cap_credit = SharePlaybackCredit {
        page_credits: 65,
        ..Default::default()
    };
    for i in 0..64 {
        cap_credit.insert_session(format!("sid_{i}"), 2000 + i as u64, 1000);
    }
    assert_eq!(cap_credit.page_credits, 65);
    cap_credit.insert_session("sid_64".to_string(), 3000, 1000);
    assert_eq!(
        cap_credit.page_credits, 64,
        "evicting oldest unclaimed session at 64-cap must decrement page_credits"
    );

    // End-to-end: GET /share/{tok} on max_views=1 followed by sid expiry does NOT leak page_credits
    let resp_ttl = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r8_ttl","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let ttl_json: Value =
        serde_json::from_slice(&to_bytes(resp_ttl.into_body(), 4096).await.unwrap()).unwrap();
    let ttl_jti = ttl_json["id"].as_str().unwrap().to_string();
    let ttl_tok = ttl_json["token"].as_str().unwrap().to_string();

    let page_ttl = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ttl_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page_ttl.status(), StatusCode::OK);
    {
        let mut map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get_mut(&ttl_jti).unwrap();
        assert_eq!(entry.page_credits, 1);
        for (exp, _) in entry.sessions.values_mut() {
            *exp = now.saturating_sub(5);
        }
    }
    // Un-sid GET /share/{ttl_tok}/video.mp4 after sid expiry must be rejected with 410 GONE!
    let r_after_sid_exp = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ttl_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_after_sid_exp.status(),
        StatusCode::GONE,
        "expired unclaimed sid session must not leave a leaked page_credit allowing max_views=1 bypass"
    );
    assert!(
        !has_active_or_in_flight_share_for_target(&state, "exp_r8_ttl", unix_now()),
        "expired unclaimed sid session must not keep target marked in-flight"
    );

    // 1b. Double-spend prevention between un-sid request and ?sid= request on max_views=1
    // Direction A: un-sid video fetch first -> subsequent ?sid= fetch is rejected with 410 GONE
    let resp_ds1 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r8_ds1","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let ds1_json: Value =
        serde_json::from_slice(&to_bytes(resp_ds1.into_body(), 4096).await.unwrap()).unwrap();
    let ds1_tok = ds1_json["token"].as_str().unwrap().to_string();
    let ds1_page = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ds1_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let ds1_html = String::from_utf8(
        to_bytes(ds1_page.into_body(), 16384)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let ds1_sid = extract_sid(&ds1_html, &ds1_tok);

    let r_ds1_unsid = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ds1_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ds1_unsid.status(), StatusCode::OK);

    let r_ds1_sid_after = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ds1_tok}/video.mp4?sid={ds1_sid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_ds1_sid_after.status(),
        StatusCode::GONE,
        "once un-sid request consumes the page_credit on max_views=1, ?sid= must not double-spend"
    );

    // Direction B: ?sid= video fetch first -> subsequent un-sid fetch is rejected with 410 GONE
    let resp_ds2 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r8_ds2","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let ds2_json: Value =
        serde_json::from_slice(&to_bytes(resp_ds2.into_body(), 4096).await.unwrap()).unwrap();
    let ds2_tok = ds2_json["token"].as_str().unwrap().to_string();
    let ds2_page = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ds2_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let ds2_html = String::from_utf8(
        to_bytes(ds2_page.into_body(), 16384)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let ds2_sid = extract_sid(&ds2_html, &ds2_tok);

    let r_ds2_sid = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ds2_tok}/video.mp4?sid={ds2_sid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ds2_sid.status(), StatusCode::OK);

    let r_ds2_unsid_after = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ds2_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ds2_unsid_after.status(), StatusCode::GONE);

    // 1c. Non-destructive Range continuation: uncredited third-party out-of-order Range probes
    //     are rejected with 410 GONE WITHOUT wiping the legitimate viewer's active stream cursor!
    let resp_stream = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r8_stream","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let stream_json: Value =
        serde_json::from_slice(&to_bytes(resp_stream.into_body(), 4096).await.unwrap()).unwrap();
    let stream_jti = stream_json["id"].as_str().unwrap().to_string();
    let stream_tok = stream_json["token"].as_str().unwrap().to_string();

    for valid_rng in ["bytes=0-15", "bytes=16-63"] {
        let r = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{stream_tok}/video.mp4"))
                    .header(header::RANGE, valid_rng)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    }
    // Third-party sends out-of-order Range probes (< cursor 64) -> rejected with 410 GONE
    for bad_probe in ["bytes=1-15", "bytes=16-31"] {
        let r_probe = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{stream_tok}/video.mp4"))
                    .header(header::RANGE, bad_probe)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_probe.status(), StatusCode::GONE);
    }
    // Legitimate viewer's next chunk Range: bytes=64-95 STILL succeeds (cursor was not wiped)!
    let r_legit_next = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{stream_tok}/video.mp4"))
                .header(header::RANGE, "bytes=64-95")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_legit_next.status(),
        StatusCode::PARTIAL_CONTENT,
        "third-party Range probe must not wipe legitimate viewer's active stream cursor"
    );
    // Once stream cursor TTL expires, continuation Range: bytes=96-127 is rejected with 410 GONE
    {
        let mut map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get_mut(&stream_jti).unwrap();
        for exp in &mut entry.active_stream_expires {
            *exp = now.saturating_sub(5);
        }
    }
    let r_expired_stream = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{stream_tok}/video.mp4"))
                .header(header::RANGE, "bytes=96-127")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_expired_stream.status(),
        StatusCode::GONE,
        "expired active_streams cursor must be pruned and rejected with 410 GONE"
    );

    // =========================================================================
    // 2. [FINDING-02] HTTP HEAD Requests on `/share/{token}` and `/share/{token}/video.mp4`
    //    Do Not Burn `max_views` or Mutate Playback State
    // =========================================================================
    let resp_head_share = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r8_head","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let head_json: Value =
        serde_json::from_slice(&to_bytes(resp_head_share.into_body(), 4096).await.unwrap())
            .unwrap();
    let head_jti = head_json["id"].as_str().unwrap().to_string();
    let head_tok = head_json["token"].as_str().unwrap().to_string();

    // 2a. HEAD /share/{head_tok} returns 200 OK with security headers and leaves access_count == 0
    let r_head_page = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(format!("/share/{head_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_head_page.status(), StatusCode::OK);
    assert_eq!(
        r_head_page
            .headers()
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .unwrap(),
        "nosniff"
    );
    assert_eq!(
        state.db.get_share_link(&head_jti).unwrap().unwrap()["access_count"],
        0,
        "HEAD /share/{{token}} must never increment access_count"
    );
    assert!(
        !state
            .share_engine
            .page_video_credits
            .lock()
            .unwrap()
            .contains_key(&head_jti),
        "HEAD /share/{{token}} must not mint a session or page_credit"
    );

    // 2b. HEAD /share/{head_tok}/video.mp4 (unranged & ranged) returns 200/206 without burning max_views=1
    let r_head_vid = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(format!("/share/{head_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_head_vid.status(), StatusCode::OK);
    assert_eq!(
        r_head_vid.headers().get(header::CONTENT_LENGTH).unwrap(),
        "128"
    );
    let r_head_vid_rng = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(format!("/share/{head_tok}/video.mp4"))
                .header(header::RANGE, "bytes=0-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_head_vid_rng.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        state.db.get_share_link(&head_jti).unwrap().unwrap()["access_count"],
        0,
        "HEAD /share/{{token}}/video.mp4 must never increment access_count"
    );

    // 2c. Subsequent human GET /share/{head_tok} + HEAD probe + GET /share/{head_tok}/video.mp4?sid=... all succeed!
    let r_get_page = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{head_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_get_page.status(), StatusCode::OK);
    let get_page_html = String::from_utf8(
        to_bytes(r_get_page.into_body(), 16384)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let head_sid = extract_sid(&get_page_html, &head_tok);
    assert_eq!(
        state.db.get_share_link(&head_jti).unwrap().unwrap()["access_count"],
        1
    );

    // HEAD on video (with and without sid) does not consume page_credits
    for head_uri in [
        format!("/share/{head_tok}/video.mp4"),
        format!("/share/{head_tok}/video.mp4?sid={head_sid}"),
    ] {
        let r_h = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri(&head_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_h.status(), StatusCode::OK);
    }
    assert_eq!(
        state
            .share_engine
            .page_video_credits
            .lock()
            .unwrap()
            .get(&head_jti)
            .unwrap()
            .page_credits,
        1,
        "HEAD on video must not consume page_credits"
    );

    let r_get_vid = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{head_tok}/video.mp4?sid={head_sid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_get_vid.status(), StatusCode::OK);

    // 2d. HEAD with Range on an active direct stream does not advance active_streams cursor
    let resp_head_rng = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r8_head_rng","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let head_rng_json: Value =
        serde_json::from_slice(&to_bytes(resp_head_rng.into_body(), 4096).await.unwrap()).unwrap();
    let head_rng_tok = head_rng_json["token"].as_str().unwrap().to_string();

    let r_rng_get1 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{head_rng_tok}/video.mp4"))
                .header(header::RANGE, "bytes=0-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_rng_get1.status(), StatusCode::PARTIAL_CONTENT);

    let r_rng_head2 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(format!("/share/{head_rng_tok}/video.mp4"))
                .header(header::RANGE, "bytes=16-63")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_rng_head2.status(), StatusCode::PARTIAL_CONTENT);

    let r_rng_get2 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{head_rng_tok}/video.mp4"))
                .header(header::RANGE, "bytes=16-63")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_rng_get2.status(),
        StatusCode::PARTIAL_CONTENT,
        "HEAD Range: bytes=16-63 must not advance active_streams cursor ahead of GET Range: bytes=16-63"
    );

    // =========================================================================
    // 3. [FINDING-03] Verify On-Disk Recording Segments in `resolve_share_target_metadata`
    //    & Revoke Broken Links in `cleanup_camera_share_clip_artifacts_in_range`
    // =========================================================================
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_r8_no_recs".to_string(),
            camera: "backyard".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 7000.0,
            end_time: Some(7008.0),
            has_clip: true,
            has_snapshot: true,
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
    state
        .db
        .insert_review_segment(&ninjanvr_db::ReviewItemDto {
            id: "rev_r8_no_recs".to_string(),
            camera: "backyard".to_string(),
            start_time: 7000.0,
            end_time: Some(7008.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-backyard-rev_r8_no_recs.webp"
                .to_string(),
            data: json!({}),
            has_been_reviewed: false,
        })
        .unwrap();

    // 3a. resolve_share_target_metadata returns camera ("backyard") with exists == false when no recordings exist
    let (no_rec_cam, _, _, no_rec_exists) =
        resolve_share_target_metadata(&state, "event", "ev_r8_no_recs");
    assert_eq!(no_rec_cam.as_deref(), Some("backyard"));
    assert!(!no_rec_exists);

    // Scoped operator (front_door only) still gets 403 FORBIDDEN on backyard event/review without recordings,
    // while admin gets 404 NOT_FOUND (never minting dead share links!)
    for (kind, tid) in [("event", "ev_r8_no_recs"), ("review", "rev_r8_no_recs")] {
        let r_scoped_403 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &scoped_fd_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "kind": kind, "target_id": tid }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_scoped_403.status(), StatusCode::FORBIDDEN);

        let r_admin_404 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "kind": kind, "target_id": tid }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_admin_404.status(),
            StatusCode::NOT_FOUND,
            "POST /api/share must return 404 NOT_FOUND when {kind}:{tid} has no valid recording segments on disk"
        );
    }

    // Pre-existing share link for event without recordings returns 404 on GET /share/{tok} WITHOUT burning max_views=1,
    // and verify_post_build_share_target_and_jti returns 404 and cleans up artifacts!
    state
        .db
        .insert_share_link_with_options(
            "shr_r8_no_recs",
            "event",
            "ev_r8_no_recs",
            Some("backyard"),
            "admin",
            now as f64,
            (now + 3600) as f64,
            Some(1),
        )
        .unwrap();
    let no_recs_payload = ShareTokenPayload {
        k: "event".to_string(),
        id: "ev_r8_no_recs".to_string(),
        exp: now + 3600,
        cam: Some("backyard".to_string()),
        jti: Some("shr_r8_no_recs".to_string()),
    };
    let no_recs_tok = sign_share_token(&state.jwt_secret, &no_recs_payload);
    let r_page_no_recs = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{no_recs_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_page_no_recs.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        state.db.get_share_link("shr_r8_no_recs").unwrap().unwrap()["access_count"],
        0,
        "GET /share/{{tok}} on event without recording segments must return 404 without incrementing access_count"
    );

    let (r_nr_mp4, r_nr_hash, c_nr_mp4) = seed_artifacts("ev_r8_no_recs");
    let post_build_nr =
        verify_post_build_share_target_and_jti(&state, &no_recs_payload, true, None, false)
            .expect("verify_post_build_share_target_and_jti must reject event without recordings");
    assert_eq!(post_build_nr.status(), StatusCode::NOT_FOUND);
    assert!(!r_nr_mp4.exists() && !r_nr_hash.exists() && !c_nr_mp4.exists());

    // 3b. Deleting recording segments via DELETE /api/{camera}/recordings revokes share links in SQLite
    let rec_r8_file = rec_dir.join("40.00.mp4");
    std::fs::write(&rec_r8_file, &mp4_payload).unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_r8_4000".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-04/12/front_door/40.00.mp4".to_string(),
            start_time: 4000.0,
            end_time: 4010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_r8_rec_del".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 4001.0,
            end_time: Some(4008.0),
            has_clip: true,
            has_snapshot: true,
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
    state
        .db
        .insert_review_segment(&ninjanvr_db::ReviewItemDto {
            id: "rev_r8_rec_del".to_string(),
            camera: "front_door".to_string(),
            start_time: 4001.0,
            end_time: Some(4008.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-front_door-rev_r8_rec_del.webp"
                .to_string(),
            data: json!({}),
            has_been_reviewed: false,
        })
        .unwrap();

    let resp_ev_share = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"event","target_id":"ev_r8_rec_del","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_ev_share.status(), StatusCode::OK);
    let ev_share_json: Value =
        serde_json::from_slice(&to_bytes(resp_ev_share.into_body(), 4096).await.unwrap()).unwrap();
    let ev_del_jti = ev_share_json["id"].as_str().unwrap().to_string();
    let ev_del_tok = ev_share_json["token"].as_str().unwrap().to_string();

    let resp_rev_share = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"review","target_id":"rev_r8_rec_del","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_rev_share.status(), StatusCode::OK);
    let rev_share_json: Value =
        serde_json::from_slice(&to_bytes(resp_rev_share.into_body(), 4096).await.unwrap()).unwrap();
    let rev_del_jti = rev_share_json["id"].as_str().unwrap().to_string();

    let r_del_4000 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/front_door/recordings?after=3990&before=4020")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_del_4000.status(), StatusCode::OK);
    assert!(
        state.db.is_share_link_revoked(&ev_del_jti).unwrap(),
        "deleting underlying recordings must revoke affected event share links in SQLite"
    );
    assert!(
        state.db.is_share_link_revoked(&rev_del_jti).unwrap(),
        "deleting underlying recordings must revoke affected review share links in SQLite"
    );
    let r_ev_after_rec_del = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{ev_del_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ev_after_rec_del.status(), StatusCode::GONE);

    // =========================================================================
    // 4. [FINDING-04] Unconditional SQLite Revocation & Artifact Cleanup on
    //    Creator/Quota/Expiry Failures in `verify_share_jti_with_view_mode`
    // =========================================================================
    // 4a. GET /share/{tok} (player page) immediately revokes invalid creator JTIs in SQLite,
    //     cleans up orphaned artifacts, and continues returning 403 FORBIDDEN on subsequent probes
    state
        .db
        .create_user_with_cameras("r8_role_u", "pass1234", "viewer", "*", 1000)
        .unwrap();
    for (jti, exp_id, creator) in [
        ("shr_r8_c_role", "exp_r8_c_role", "r8_role_u"),
        ("shr_r8_c_del", "exp_r8_c_del", "r8_deleted_ghost"),
        ("shr_r8_c_ext", "exp_r8_c_ext", "ext:   "),
    ] {
        state
            .db
            .insert_share_link(
                jti,
                "export",
                exp_id,
                Some("front_door"),
                creator,
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        let (r_mp4, r_hash, c_mp4) = seed_artifacts(exp_id);
        let tok = sign_share_token(
            &state.jwt_secret,
            &ShareTokenPayload {
                k: "export".to_string(),
                id: exp_id.to_string(),
                exp: now + 3600,
                cam: Some("front_door".to_string()),
                jti: Some(jti.to_string()),
            },
        );
        // First request is GET /share/{tok} (is_player_page == true)
        let r_page = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_page.status(), StatusCode::FORBIDDEN);
        assert!(
            state.db.is_share_link_revoked(jti).unwrap(),
            "GET /share/{{tok}} must immediately revoke invalid creator JTI {jti} in SQLite"
        );
        assert!(
            !r_mp4.exists() && !r_hash.exists() && !c_mp4.exists(),
            "GET /share/{{tok}} creator auto-revocation must immediately clean up artifacts for {exp_id}"
        );
        // Subsequent requests to /share/{tok} and /share/{tok}/video.mp4 continue returning 403 FORBIDDEN
        for uri in [format!("/share/{tok}"), format!("/share/{tok}/video.mp4")] {
            let r_sub = auth_app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r_sub.status(), StatusCode::FORBIDDEN);
        }
    }

    // 4b. Immediate artifact cleanup when verify_share_jti_with_view_mode rejects due to max_views or expires_at
    state
        .db
        .insert_share_link_with_options(
            "shr_r8_mv_clean",
            "export",
            "exp_r8_mv_clean",
            Some("front_door"),
            "admin",
            now as f64,
            (now + 3600) as f64,
            Some(1),
        )
        .unwrap();
    assert!(
        state
            .db
            .record_share_link_access("shr_r8_mv_clean", now as f64)
            .unwrap()
    );
    let (r_mv_mp4, r_mv_hash, c_mv_mp4) = seed_artifacts("exp_r8_mv_clean");
    let mv_clean_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_r8_mv_clean".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_r8_mv_clean".to_string()),
        },
    );
    let r_mv_gone = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{mv_clean_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_mv_gone.status(), StatusCode::GONE);
    assert!(
        !r_mv_mp4.exists() && !r_mv_hash.exists() && !c_mv_mp4.exists(),
        "exhausted max_views rejection must clean up orphaned target artifacts when no session is in flight"
    );

    state
        .db
        .insert_share_link(
            "shr_r8_exp_clean",
            "export",
            "exp_r8_exp_clean",
            Some("front_door"),
            "admin",
            (now - 3600) as f64,
            (now - 10) as f64,
        )
        .unwrap();
    let (r_ex_mp4, r_ex_hash, c_ex_mp4) = seed_artifacts("exp_r8_exp_clean");
    let exp_clean_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_r8_exp_clean".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_r8_exp_clean".to_string()),
        },
    );
    let r_exp_gone = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{exp_clean_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_exp_gone.status(), StatusCode::GONE);
    assert!(
        !r_ex_mp4.exists() && !r_ex_hash.exists() && !c_ex_mp4.exists(),
        "expired JTI rejection must clean up orphaned target artifacts"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_share_link_round10_audit_findings_regression_work() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr_sec_share_r10_{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
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
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let auth_app = build_router_with_auth(state.clone(), true);

    let now = unix_now();
    let make_jwt = |sub: &str, role: &str, allowed_cameras: Option<&str>| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: allowed_cameras.map(str::to_string),
                iat: unix_now(),
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };

    let admin_jwt = make_jwt("admin", "admin", Some("*"));
    let admin_bearer = format!("Bearer {admin_jwt}");
    let admin_cookie = format!("ninjanvr_token={admin_jwt}");
    state
        .db
        .create_user_with_cameras("scoped_r10_fd", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    let scoped_fd_jwt = make_jwt("scoped_r10_fd", "operator", Some("front_door"));
    let scoped_fd_bearer = format!("Bearer {scoped_fd_jwt}");

    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    let rec_dir = media_dir.join("recordings/2026-11-04/12/front_door");
    std::fs::create_dir_all(&exports_dir).unwrap();
    std::fs::create_dir_all(&clips_dir).unwrap();
    std::fs::create_dir_all(&rec_dir).unwrap();

    let mp4_payload: Vec<u8> = (0u8..128u8).collect();
    for exp_id in [
        "exp_r10_clamped1",
        "exp_r10_clamped2",
        "exp_r10_auto_keep",
        "exp_r10_auto_u",
        "exp_r10_auto_del",
        "exp_r10_auto_tok",
        "exp_r10_stale_red",
        "exp_r10_stale_clip",
        "exp_r10_rename",
    ] {
        std::fs::write(exports_dir.join(format!("{exp_id}.mp4")), &mp4_payload).unwrap();
        state
            .db
            .insert_export(&ExportRow {
                id: exp_id.to_string(),
                camera: "front_door".to_string(),
                name: format!("R10 {exp_id}"),
                date: 1700000100.0,
                video_path: format!("/media/ninjanvr/exports/{exp_id}.mp4"),
                thumb_path: format!("/media/ninjanvr/clips/{exp_id}.webp"),
                in_progress: false,
            })
            .unwrap();
    }

    // =========================================================================
    // 1. [R10-FINDING-01] Over-Clamped Initial Range Requests & Clamped Cursor Update
    // =========================================================================
    // 1a. Bounded initial `Range: bytes=0-65535` on a 128-byte MP4 (`Content-Range: bytes 0-127/128`)
    //     returns `(true, 2)` in `next_stream_state_from_video_response` and seeds `active_streams = [2]`
    //     on a `max_views = 1` link so subsequent range requests succeed!
    let mut init_clamp_hdrs = HeaderMap::new();
    init_clamp_hdrs.insert(header::RANGE, "bytes=0-65535".parse().unwrap());
    let resp_unit_clamp = serve_mp4_bytes_with_range(mp4_payload.clone(), &init_clamp_hdrs);
    assert_eq!(resp_unit_clamp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        next_stream_state_from_video_response(&resp_unit_clamp, Some((0, Some(65535)))),
        (true, 2)
    );

    let resp_share_c1 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r10_clamped1","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let c1_json: Value =
        serde_json::from_slice(&to_bytes(resp_share_c1.into_body(), 4096).await.unwrap()).unwrap();
    let c1_jti = c1_json["id"].as_str().unwrap().to_string();
    let c1_tok = c1_json["token"].as_str().unwrap().to_string();

    for rng in ["bytes=0-65535", "bytes=16-63", "bytes=64-127"] {
        let r = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{c1_tok}/video.mp4"))
                    .header(header::RANGE, rng)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::PARTIAL_CONTENT,
            "Range {rng} after clamped initial Range: bytes=0-65535 must succeed on max_views=1"
        );
    }
    assert_eq!(
        state.db.get_share_link(&c1_jti).unwrap().unwrap()["access_count"],
        1
    );

    // 1b. 2-byte initial probe `Range: bytes=0-1` followed by over-clamped `Range: bytes=0-65535`
    //     updates `active_streams[pos]` to response-clamped `min_next_start` (2) rather than unclamped 65536,
    //     so subsequent `Range: bytes=16-63` succeeds!
    let resp_share_c2 = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/share")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"kind":"export","target_id":"exp_r10_clamped2","expires_in_sec":3600,"max_views":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    let c2_json: Value =
        serde_json::from_slice(&to_bytes(resp_share_c2.into_body(), 4096).await.unwrap()).unwrap();
    let c2_jti = c2_json["id"].as_str().unwrap().to_string();
    let c2_tok = c2_json["token"].as_str().unwrap().to_string();

    for rng in ["bytes=0-1", "bytes=0-65535", "bytes=16-63"] {
        let r = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/share/{c2_tok}/video.mp4"))
                    .header(header::RANGE, rng)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::PARTIAL_CONTENT,
            "Range {rng} in 2-byte probe -> over-clamped 0-65535 -> 16-63 sequence must succeed"
        );
    }
    assert_eq!(
        state
            .share_engine
            .page_video_credits
            .lock()
            .unwrap()
            .get(&c2_jti)
            .unwrap()
            .active_streams,
        vec![64]
    );

    // =========================================================================
    // 2. [R10-FINDING-02] Bounded Selective Eviction & Pruning of `share_creator_auto_revoked`
    // =========================================================================
    // 2a. `prune_share_creator_auto_revoked` and `purge_revoked_share_credits` prune expired/deleted JTIs
    //     while retaining active-TTL auto-revoked JTIs
    state
        .db
        .insert_share_link(
            "shr_r10_auto_keep",
            "export",
            "exp_r10_auto_keep",
            Some("front_door"),
            "ghost_deleted_creator",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let keep_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_r10_auto_keep".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_r10_auto_keep".to_string()),
        },
    );
    // Trigger auto-revocation on `shr_r10_auto_keep` -> 403 FORBIDDEN & stored in `share_creator_auto_revoked`
    let r_keep_init = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{keep_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_keep_init.status(), StatusCode::FORBIDDEN);
    assert!(
        state
            .share_engine
            .creator_auto_revoked
            .lock()
            .unwrap()
            .contains_key("shr_r10_auto_keep")
    );

    // Seed 511 synthetic expired/missing JTIs in `share_creator_auto_revoked` (bringing len to 512)
    {
        let mut auto_map = state.share_engine.creator_auto_revoked.lock().unwrap();
        for i in 0..511 {
            auto_map.insert(
                format!("shr_r10_stale_{i}"),
                "Share link creator user no longer exists",
            );
        }
        assert_eq!(auto_map.len(), 512);
    }
    // `purge_revoked_share_credits` prunes the 511 missing/expired JTIs while keeping `shr_r10_auto_keep`
    purge_revoked_share_credits(&state);
    {
        let auto_map = state.share_engine.creator_auto_revoked.lock().unwrap();
        assert_eq!(auto_map.len(), 1);
        assert!(auto_map.contains_key("shr_r10_auto_keep"));
    }

    // 2b. When 512 active-TTL entries exist in `share_creator_auto_revoked`, inserting a 513th
    //     auto-revoked JTI evicts only a single entry (NOT `auto_map.clear()`), keeping map len at 512!
    {
        let mut auto_map = state.share_engine.creator_auto_revoked.lock().unwrap();
        for i in 0..511 {
            let jti_k = format!("shr_r10_cap_{i}");
            state
                .db
                .insert_share_link(
                    &jti_k,
                    "export",
                    "exp_r10_auto_keep",
                    Some("front_door"),
                    "ghost_deleted_creator",
                    now as f64,
                    (now + 3600) as f64,
                )
                .unwrap();
            let _ = state.db.revoke_share_link(&jti_k);
            auto_map.insert(jti_k, "Share link creator user no longer exists");
        }
        assert_eq!(auto_map.len(), 512);
    }
    state
        .db
        .insert_share_link(
            "shr_r10_cap_512",
            "export",
            "exp_r10_auto_keep",
            Some("front_door"),
            "ghost_deleted_creator",
            now as f64,
            (now + 3600) as f64,
        )
        .unwrap();
    let cap512_tok = sign_share_token(
        &state.jwt_secret,
        &ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_r10_auto_keep".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_r10_cap_512".to_string()),
        },
    );
    let r_cap512 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{cap512_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_cap512.status(), StatusCode::FORBIDDEN);
    {
        let mut auto_map = state.share_engine.creator_auto_revoked.lock().unwrap();
        assert_eq!(
            auto_map.len(),
            512,
            "512-cap eviction must evict a single entry instead of clearing all 512 entries"
        );
        assert!(auto_map.contains_key("shr_r10_cap_512"));
        auto_map.retain(|k, _| k == "shr_r10_auto_keep");
        auto_map.insert(
            "shr_r10_auto_keep".to_string(),
            "Share link creator user no longer exists",
        );
    }

    // 2c. `api_user_role`, `api_user_delete`, and `api_token_delete` selectively remove only the
    //     mutated/deleted creator's entries from `share_creator_auto_revoked`, preserving `shr_r10_auto_keep`!
    state
        .db
        .create_user_with_cameras("r10_u_role", "pass1234", "operator", "*", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("r10_u_del", "pass1234", "operator", "*", 1000)
        .unwrap();
    let (tok_r10_meta, _) = state
        .db
        .create_api_token("r10_tok_del", "full", Some((now + 3600) as f64))
        .unwrap();
    let tok_r10_id = tok_r10_meta["id"].as_str().unwrap().to_string();

    for (jti, exp_id, creator) in [
        ("shr_r10_u_role", "exp_r10_auto_u", "r10_u_role"),
        ("shr_r10_u_del", "exp_r10_auto_del", "ext:r10_u_del"),
        ("shr_r10_tok_del", "exp_r10_auto_tok", "token:r10_tok_del"),
    ] {
        state
            .db
            .insert_share_link(
                jti,
                "export",
                exp_id,
                Some("front_door"),
                creator,
                now as f64,
                (now + 3600) as f64,
            )
            .unwrap();
        state
            .share_engine
            .creator_auto_revoked
            .lock()
            .unwrap()
            .insert(jti.to_string(), "Share link creator camera access revoked");
    }

    let r_role = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/users/r10_u_role/role")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"role":"viewer"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_role.status(), StatusCode::OK);
    {
        let auto_map = state.share_engine.creator_auto_revoked.lock().unwrap();
        assert!(!auto_map.contains_key("shr_r10_u_role"));
        assert!(auto_map.contains_key("shr_r10_u_del"));
        assert!(auto_map.contains_key("shr_r10_tok_del"));
        assert!(auto_map.contains_key("shr_r10_auto_keep"));
    }

    let r_udel = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/users/r10_u_del")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_udel.status(), StatusCode::OK);
    {
        let auto_map = state.share_engine.creator_auto_revoked.lock().unwrap();
        assert!(!auto_map.contains_key("shr_r10_u_del"));
        assert!(auto_map.contains_key("shr_r10_tok_del"));
        assert!(auto_map.contains_key("shr_r10_auto_keep"));
    }

    let r_tdel = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/tokens/{tok_r10_id}"))
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_tdel.status(), StatusCode::OK);
    {
        let auto_map = state.share_engine.creator_auto_revoked.lock().unwrap();
        assert!(!auto_map.contains_key("shr_r10_tok_del"));
        assert!(auto_map.contains_key("shr_r10_auto_keep"));
    }

    // `shr_r10_auto_keep` still returns 403 FORBIDDEN after all unrelated user/token mutations!
    let r_keep_after = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{keep_tok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_keep_after.status(), StatusCode::FORBIDDEN);

    // =========================================================================
    // 3. [R10-FINDING-03] Invalidate Stale Cached Share Clips on Source mtime Update
    //    & Verify Audio-Free Redacted/Clip Cache
    // =========================================================================
    let make_mp4_box = |box_type: &[u8; 4], payload: &[u8]| -> Vec<u8> {
        let total = (8 + payload.len()) as u32;
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(box_type);
        out.extend_from_slice(payload);
        out
    };
    let make_mp4_full_box = |box_type: &[u8; 4], version: u8, flags: u32, body: &[u8]| -> Vec<u8> {
        let total = (12 + body.len()) as u32;
        let mut out = Vec::with_capacity(12 + body.len());
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(box_type);
        let vf = ((version as u32) << 24) | (flags & 0x00FF_FFFF);
        out.extend_from_slice(&vf.to_be_bytes());
        out.extend_from_slice(body);
        out
    };
    let build_mp4_with_audio = || -> Vec<u8> {
        let ftyp = make_mp4_box(b"ftyp", b"isom\x00\x00\x02\x00isom");
        let mdat = make_mp4_box(b"mdat", &[0x11u8; 32]);
        let mut hdlr_body = vec![0u8; 20];
        hdlr_body[4..8].copy_from_slice(b"soun");
        let hdlr = make_mp4_full_box(b"hdlr", 0, 0, &hdlr_body);
        let mdia = make_mp4_box(b"mdia", &hdlr);
        let soun_trak = make_mp4_box(b"trak", &mdia);
        let moov = make_mp4_box(b"moov", &soun_trak);
        let mut out = Vec::new();
        out.extend_from_slice(&ftyp);
        out.extend_from_slice(&mdat);
        out.extend_from_slice(&moov);
        out
    };
    let audio_mp4 = build_mp4_with_audio();

    // 3a. Export redacted cache (`share_redacted_exp_r10_stale_red.mp4`):
    //     invalidated when contains audio track OR when `source_path` mtime is newer than `redacted_path`!
    let mask_val = json!([[[0.1, 0.1], [0.4, 0.1], [0.4, 0.4], [0.1, 0.4]]]);
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask = mask_val.clone();
    }
    let resp_sh_red = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_r10_stale_red","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let sh_red_tok =
        serde_json::from_slice::<Value>(&to_bytes(resp_sh_red.into_body(), 4096).await.unwrap())
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();

    let red_art_path = exports_dir.join("share_redacted_exp_r10_stale_red.mp4");
    let red_hash_path = exports_dir.join(".share_redacted_exp_r10_stale_red.mask_hash");
    let src_red_path = exports_dir.join("exp_r10_stale_red.mp4");

    // Pre-seed cached redacted file containing an audio track -> invalidated and removed!
    std::fs::write(&red_art_path, &audio_mp4).unwrap();
    std::fs::write(&red_hash_path, compute_privacy_mask_hash(Some(&mask_val))).unwrap();
    let r_red_audio = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{sh_red_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_red_audio.status(), StatusCode::NOT_FOUND);
    assert!(
        !red_art_path.exists(),
        "cached redacted MP4 containing audio track must be invalidated and removed"
    );

    // Pre-seed valid silent redacted file, then touch/update source export with newer mtime -> invalidated!
    std::fs::write(&red_art_path, b"SILENT_REDACTED_V1").unwrap();
    std::fs::write(&red_hash_path, compute_privacy_mask_hash(Some(&mask_val))).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    std::fs::write(&src_red_path, &mp4_payload).unwrap();
    assert!(is_cached_share_artifact_stale(
        &red_art_path,
        std::slice::from_ref(&src_red_path)
    ));
    let r_red_stale = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{sh_red_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_red_stale.status(), StatusCode::NOT_FOUND);
    assert!(
        !red_art_path.exists(),
        "stale cached redacted MP4 older than source_path must be invalidated and removed"
    );
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask = Value::Null;
    }

    // 3b. Export audio-stripped cache (`share_clip_exp_r10_stale_clip.mp4`):
    //     invalidated when `source_path` mtime is newer than `clip_path`!
    let src_clip_path = exports_dir.join("exp_r10_stale_clip.mp4");
    std::fs::write(&src_clip_path, &audio_mp4).unwrap();
    let resp_sh_clip = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"export","target_id":"exp_r10_stale_clip","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let sh_clip_tok =
        serde_json::from_slice::<Value>(&to_bytes(resp_sh_clip.into_body(), 4096).await.unwrap())
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();

    let clip_art_path = exports_dir.join("share_clip_exp_r10_stale_clip.mp4");
    std::fs::write(&clip_art_path, b"SILENT_STRIPPED_V1").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    std::fs::write(&src_clip_path, &audio_mp4).unwrap();
    assert!(is_cached_share_artifact_stale(
        &clip_art_path,
        std::slice::from_ref(&src_clip_path)
    ));
    let r_clip_stale = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{sh_clip_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_clip_stale.status(), StatusCode::NOT_FOUND);
    assert!(
        !clip_art_path.exists(),
        "stale share_clip_<id>.mp4 older than source export must be invalidated and removed"
    );

    // 3c. Event/review unmasked (`share_clip_ev_r10_1.mp4`) & redacted (`share_redacted_ev_r10_1.mp4`):
    //     invalidated when contains audio track OR when recording segment mtime is newer!
    let rec_r10_seg = rec_dir.join("50.00.mp4");
    std::fs::write(&rec_r10_seg, &mp4_payload).unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_r10_5000".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-04/12/front_door/50.00.mp4".to_string(),
            start_time: 5000.0,
            end_time: 5010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_r10_1".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 5001.0,
            end_time: Some(5008.0),
            has_clip: true,
            has_snapshot: true,
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
    let resp_sh_ev = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/share")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"kind":"event","target_id":"ev_r10_1","expires_in_sec":3600}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let sh_ev_tok =
        serde_json::from_slice::<Value>(&to_bytes(resp_sh_ev.into_body(), 4096).await.unwrap())
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();

    let ev_clip_path = exports_dir.join("share_clip_ev_r10_1.mp4");
    // Cached `share_clip_ev_r10_1.mp4` with audio track -> invalidated and rebuilt from `rec_r10_seg`!
    std::fs::write(&ev_clip_path, &audio_mp4).unwrap();
    let r_ev_clip_audio = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{sh_ev_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ev_clip_audio.status(), StatusCode::OK);
    let ev_audio_rebuilt = to_bytes(r_ev_clip_audio.into_body(), 4096).await.unwrap();
    assert_eq!(
        &ev_audio_rebuilt[..],
        &mp4_payload[..],
        "event share_clip_<id>.mp4 containing audio track must be invalidated and rebuilt from source segment"
    );

    // Cached `share_clip_ev_r10_1.mp4` older than recording segment `rec_r10_seg` -> invalidated and rebuilt!
    std::fs::write(&ev_clip_path, b"SILENT_EV_CLIP_V1").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let updated_seg_bytes = b"UPDATED_REC_SEG_V2_PAYLOAD_BYTES".to_vec();
    std::fs::write(&rec_r10_seg, &updated_seg_bytes).unwrap();
    let r_ev_clip_stale = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{sh_ev_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ev_clip_stale.status(), StatusCode::OK);
    let ev_stale_rebuilt = to_bytes(r_ev_clip_stale.into_body(), 4096).await.unwrap();
    assert_eq!(
        &ev_stale_rebuilt[..],
        &updated_seg_bytes[..],
        "stale event share_clip_<id>.mp4 older than recording segment must be invalidated and rebuilt"
    );

    // Also verify event redacted (`share_redacted_ev_r10_1.mp4`) invalidates on audio track & stale mtime
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask = mask_val.clone();
    }
    let ev_red_path = exports_dir.join("share_redacted_ev_r10_1.mp4");
    let ev_red_hash = exports_dir.join(".share_redacted_ev_r10_1.mask_hash");
    std::fs::write(&ev_red_path, &audio_mp4).unwrap();
    std::fs::write(&ev_red_hash, compute_privacy_mask_hash(Some(&mask_val))).unwrap();
    let r_ev_red_audio = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{sh_ev_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ev_red_audio.status(), StatusCode::NOT_FOUND);
    assert!(!ev_red_path.exists());

    std::fs::write(&ev_red_path, b"SILENT_EV_REDACTED_V1").unwrap();
    std::fs::write(&ev_red_hash, compute_privacy_mask_hash(Some(&mask_val))).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    std::fs::write(&rec_r10_seg, &updated_seg_bytes).unwrap();
    let r_ev_red_stale = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{sh_ev_tok}/video.mp4"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ev_red_stale.status(), StatusCode::NOT_FOUND);
    assert!(!ev_red_path.exists());
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask = Value::Null;
    }

    // 3d. `api_export_create` (`POST /api/export/front_door/start/5000/end/5010`) purges cached share artifacts!
    let reexport_id = "front_door_5000_5010";
    let reexport_red = exports_dir.join(format!("share_redacted_{reexport_id}.mp4"));
    let reexport_hash = exports_dir.join(format!(".share_redacted_{reexport_id}.mask_hash"));
    let reexport_clip = exports_dir.join(format!("share_clip_{reexport_id}.mp4"));
    std::fs::write(&reexport_red, b"STALE_RED").unwrap();
    std::fs::write(&reexport_hash, b"STALE_HASH").unwrap();
    std::fs::write(&reexport_clip, b"STALE_CLIP").unwrap();

    let _ = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/export/front_door/start/5000/end/5010")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !reexport_red.exists() && !reexport_hash.exists() && !reexport_clip.exists(),
        "api_export_create must purge any stale share_redacted_* and share_clip_* artifacts for the export ID"
    );

    // =========================================================================
    // 4. [R10-FINDING-04] Cookie CSRF Guard & Handler Camera Scoping on
    //    `api_export_create`, `api_export_rename`, `api_config_save`, and `api_config_structured`
    // =========================================================================
    for (method, uri, content_type, body) in [
        (
            "POST",
            "/api/export/front_door/start/5000/end/5010",
            "application/json",
            r#"{"name":"csrf"}"#,
        ),
        (
            "PATCH",
            "/api/export/exp_r10_rename/rename",
            "application/json",
            r#"{"name":"csrf_renamed"}"#,
        ),
        (
            "POST",
            "/api/config/save",
            "text/plain",
            "version: 0.18-0\n",
        ),
        (
            "PUT",
            "/api/config/structured",
            "application/json",
            r#"{"cameras":{}}"#,
        ),
    ] {
        let r_csrf = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::COOKIE, &admin_cookie)
                    .header(header::ORIGIN, "https://evil.example")
                    .header(header::HOST, "nvr.local:5000")
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_csrf.status(),
            StatusCode::FORBIDDEN,
            "Cross-origin cookie request to {method} {uri} must be rejected with 403 FORBIDDEN"
        );
    }

    // Handler-level camera scoping in `api_export_create` blocks percent-encoded unauthorized camera
    let r_enc_cam = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/export/%62ackyard/start/5000/end/5010")
                .header(header::AUTHORIZATION, &scoped_fd_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_enc_cam.status(),
        StatusCode::FORBIDDEN,
        "api_export_create must enforce caller_allowed_cameras on percent-encoded camera parameter"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_share_link_round12_audit_findings_regression_work() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr_sec_share_r12_{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
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
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let auth_app = build_router_with_auth(state.clone(), true);

    let now = unix_now();
    let admin_jwt = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &JwtClaims {
            sub: "admin".to_string(),
            role: "admin".to_string(),
            allowed_cameras: Some("*".to_string()),
            iat: now,
            exp: now + 3600,
        },
        &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .unwrap();
    let admin_bearer = format!("Bearer {admin_jwt}");

    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    let rec_dir = media_dir.join("recordings/2026-11-04/12/front_door");
    std::fs::create_dir_all(&exports_dir).unwrap();
    std::fs::create_dir_all(&clips_dir).unwrap();
    std::fs::create_dir_all(&rec_dir).unwrap();

    let mp4_payload: Vec<u8> = (0u8..128u8).collect();
    let make_mp4_box = |box_type: &[u8; 4], payload: &[u8]| -> Vec<u8> {
        let total = (8 + payload.len()) as u32;
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(box_type);
        out.extend_from_slice(payload);
        out
    };
    let make_mp4_full_box = |box_type: &[u8; 4], version: u8, flags: u32, body: &[u8]| -> Vec<u8> {
        let total = (12 + body.len()) as u32;
        let mut out = Vec::with_capacity(12 + body.len());
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(box_type);
        let vf = ((version as u32) << 24) | (flags & 0x00FF_FFFF);
        out.extend_from_slice(&vf.to_be_bytes());
        out.extend_from_slice(body);
        out
    };
    let audio_mp4 = {
        let ftyp = make_mp4_box(b"ftyp", b"isom\x00\x00\x02\x00isom");
        let mdat = make_mp4_box(b"mdat", &[0x11u8; 32]);
        let mut hdlr_body = vec![0u8; 20];
        hdlr_body[4..8].copy_from_slice(b"soun");
        let hdlr = make_mp4_full_box(b"hdlr", 0, 0, &hdlr_body);
        let mdia = make_mp4_box(b"mdia", &hdlr);
        let soun_trak = make_mp4_box(b"trak", &mdia);
        let moov = make_mp4_box(b"moov", &soun_trak);
        let mut out = Vec::new();
        out.extend_from_slice(&ftyp);
        out.extend_from_slice(&mdat);
        out.extend_from_slice(&moov);
        out
    };

    for exp_id in [
        "exp_r12_head_red",
        "exp_r12_head_aud",
        "exp_r12_rollback",
        "exp_r12_touch1",
        "exp_r12_touch2",
    ] {
        let bytes = if exp_id == "exp_r12_head_aud" {
            &audio_mp4
        } else {
            &mp4_payload
        };
        std::fs::write(exports_dir.join(format!("{exp_id}.mp4")), bytes).unwrap();
        state
            .db
            .insert_export(&ExportRow {
                id: exp_id.to_string(),
                camera: "front_door".to_string(),
                name: format!("R12 {exp_id}"),
                date: 1700000200.0,
                video_path: format!("/media/ninjanvr/exports/{exp_id}.mp4"),
                thumb_path: format!("/media/ninjanvr/clips/{exp_id}.webp"),
                in_progress: false,
            })
            .unwrap();
    }

    let rec_r12_seg = rec_dir.join("55.00.mp4");
    std::fs::write(&rec_r12_seg, &mp4_payload).unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_r12_6000".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-04/12/front_door/55.00.mp4".to_string(),
            start_time: 6000.0,
            end_time: 6010.0,
            duration: 10.0,
            motion: 5,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_r12_head".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 6001.0,
            end_time: Some(6008.0),
            has_clip: true,
            has_snapshot: true,
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

    let mint_share = |kind: &str, target_id: &str, max_views: Option<u32>| {
        let auth_app = auth_app.clone();
        let admin_bearer = admin_bearer.clone();
        let body = json!({
            "kind": kind,
            "target_id": target_id,
            "expires_in_sec": 3600,
            "max_views": max_views,
        })
        .to_string();
        async move {
            let r = auth_app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/share")
                        .header(header::AUTHORIZATION, &admin_bearer)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            let v: Value =
                serde_json::from_slice(&to_bytes(r.into_body(), 4096).await.unwrap()).unwrap();
            (
                v["id"].as_str().unwrap().to_string(),
                v["token"].as_str().unwrap().to_string(),
            )
        }
    };

    // =========================================================================
    // 1. [R12-FINDING-01] Prevent Uncounted FFmpeg Transcode DoS on HTTP HEAD
    // =========================================================================
    // 1a. Unit check `build_share_video_head_placeholder_response`
    let unranged_placeholder = build_share_video_head_placeholder_response(&HeaderMap::new());
    assert_eq!(unranged_placeholder.status(), StatusCode::OK);
    assert_eq!(
        unranged_placeholder
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap(),
        "video/mp4"
    );
    assert_eq!(
        unranged_placeholder
            .headers()
            .get(header::ACCEPT_RANGES)
            .unwrap(),
        "bytes"
    );
    assert_eq!(
        unranged_placeholder
            .headers()
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .unwrap(),
        "nosniff"
    );
    let mut ranged_hdrs = HeaderMap::new();
    ranged_hdrs.insert(header::RANGE, "bytes=0-15".parse().unwrap());
    let ranged_placeholder = build_share_video_head_placeholder_response(&ranged_hdrs);
    assert_eq!(ranged_placeholder.status(), StatusCode::PARTIAL_CONTENT);

    // Hold ALL 2 global `share_ffmpeg_semaphore` permits so any attempt to acquire the semaphore
    // in `HEAD /share/{token}/video.mp4` would immediately deadlock!
    let held_ffmpeg_permits = state
        .share_engine
        .ffmpeg_semaphore
        .acquire_many(2)
        .await
        .expect("must acquire both FFmpeg semaphore permits");

    let (jti_exp_red, tok_exp_red) = mint_share("export", "exp_r12_head_red", Some(1)).await;
    let (jti_exp_aud, tok_exp_aud) = mint_share("export", "exp_r12_head_aud", Some(1)).await;
    let (jti_ev_head, tok_ev_head) = mint_share("event", "ev_r12_head", Some(1)).await;

    let valid_mask = json!([[[0.1, 0.1], [0.4, 0.1], [0.4, 0.4], [0.1, 0.4]]]);
    let invalid_mask = json!([{"not_a_polygon": true}]);

    // Branch 1 (export redacted) & Branch 3 (event redacted) with uncompilable privacy mask -> 404 NOT_FOUND on HEAD
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask = invalid_mask;
    }
    for tok in [&tok_exp_red, &tok_ev_head] {
        let r_bad_mask = tokio::time::timeout(
            Duration::from_secs(2),
            auth_app.clone().oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri(format!("/share/{tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .expect("HEAD must not block on share_ffmpeg_semaphore")
        .unwrap();
        assert_eq!(
            r_bad_mask.status(),
            StatusCode::NOT_FOUND,
            "HEAD on link with uncompilable privacy mask must return 404 NOT_FOUND"
        );
    }

    // Branch 1 (export redacted) & Branch 3 (event redacted) with valid privacy mask, uncached on disk:
    // HEAD returns 200 OK / 206 PARTIAL_CONTENT placeholder WITHOUT acquiring semaphore or spawning FFmpeg!
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask = valid_mask.clone();
    }
    for (jti, tok, tid) in [
        (&jti_exp_red, &tok_exp_red, "exp_r12_head_red"),
        (&jti_ev_head, &tok_ev_head, "ev_r12_head"),
    ] {
        let r_head_ok = tokio::time::timeout(
            Duration::from_secs(2),
            auth_app.clone().oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri(format!("/share/{tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .expect("HEAD must not acquire share_ffmpeg_semaphore")
        .unwrap();
        assert_eq!(r_head_ok.status(), StatusCode::OK);
        assert_eq!(
            r_head_ok.headers().get(header::CONTENT_TYPE).unwrap(),
            "video/mp4"
        );
        assert_eq!(
            r_head_ok.headers().get(header::ACCEPT_RANGES).unwrap(),
            "bytes"
        );

        let r_head_206 = tokio::time::timeout(
            Duration::from_secs(2),
            auth_app.clone().oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri(format!("/share/{tok}/video.mp4"))
                    .header(header::RANGE, "bytes=0-15")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .expect("ranged HEAD must not acquire share_ffmpeg_semaphore")
        .unwrap();
        assert_eq!(r_head_206.status(), StatusCode::PARTIAL_CONTENT);
        assert!(
            !exports_dir
                .join(format!("share_redacted_{tid}.mp4"))
                .exists(),
            "HEAD must not spawn FFmpeg to build share_redacted_{tid}.mp4"
        );
        assert_eq!(
            state.db.get_share_link(jti).unwrap().unwrap()["access_count"],
            0
        );

        // When valid cached redacted file exists on disk, HEAD serves its exact headers (`Content-Length: 128`)
        let red_path = exports_dir.join(format!("share_redacted_{tid}.mp4"));
        let hash_path = exports_dir.join(format!(".share_redacted_{tid}.mask_hash"));
        std::fs::write(&red_path, &mp4_payload).unwrap();
        std::fs::write(&hash_path, compute_privacy_mask_hash(Some(&valid_mask))).unwrap();
        let r_head_cached = tokio::time::timeout(
            Duration::from_secs(2),
            auth_app.clone().oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri(format!("/share/{tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(r_head_cached.status(), StatusCode::OK);
        assert_eq!(
            r_head_cached.headers().get(header::CONTENT_LENGTH).unwrap(),
            "128"
        );
        let _ = std::fs::remove_file(&red_path);
        let _ = std::fs::remove_file(&hash_path);
    }

    // Clear privacy mask to test Branch 2 (export audio-stripped) & Branch 4 (event unmasked clip)
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask = Value::Null;
    }
    for (jti, tok, tid) in [
        (&jti_exp_aud, &tok_exp_aud, "exp_r12_head_aud"),
        (&jti_ev_head, &tok_ev_head, "ev_r12_head"),
    ] {
        let r_head_ok = tokio::time::timeout(
            Duration::from_secs(2),
            auth_app.clone().oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri(format!("/share/{tok}/video.mp4"))
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .expect("HEAD on uncached clip branch must not acquire share_ffmpeg_semaphore")
        .unwrap();
        assert_eq!(r_head_ok.status(), StatusCode::OK);

        let r_head_206 = tokio::time::timeout(
            Duration::from_secs(2),
            auth_app.clone().oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri(format!("/share/{tok}/video.mp4"))
                    .header(header::RANGE, "bytes=0-15")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .expect("ranged HEAD on uncached clip branch must not acquire share_ffmpeg_semaphore")
        .unwrap();
        assert_eq!(r_head_206.status(), StatusCode::PARTIAL_CONTENT);
        assert!(
            !exports_dir.join(format!("share_clip_{tid}.mp4")).exists(),
            "HEAD must not spawn FFmpeg to build share_clip_{tid}.mp4"
        );
        assert_eq!(
            state.db.get_share_link(jti).unwrap().unwrap()["access_count"],
            0
        );
    }

    drop(held_ffmpeg_permits);

    // =========================================================================
    // 2. [R12-FINDING-02] Roll Back `prev_stream_cursor` & `active_stream_expires`
    //    on All Non-2xx Early-Return Errors in `share_video_mp4`
    // =========================================================================
    let (jti_rb, tok_rb) = mint_share("export", "exp_r12_rollback", Some(1)).await;
    // Start initial ranged stream `bytes=0-15` -> `active_streams == [16]`, `access_count == 1`
    let r_rb_init = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_rb}/video.mp4"))
                .header(header::RANGE, "bytes=0-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_rb_init.status(), StatusCode::PARTIAL_CONTENT);

    // Pin `active_stream_expires[0]` to a distinct value `now + 300` so we can verify expiry rollback
    let custom_exp = now + 300;
    {
        let mut map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get_mut(&jti_rb).unwrap();
        assert_eq!(entry.active_streams, vec![16]);
        entry.active_stream_expires = vec![custom_exp];
    }

    // 2a. `416 RANGE_NOT_SATISFIABLE` (`bytes=999999-`) rolls back BOTH `active_streams` AND `active_stream_expires`
    let r_rb_416 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_rb}/video.mp4"))
                .header(header::RANGE, "bytes=999999-")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_rb_416.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    {
        let map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get(&jti_rb).unwrap();
        assert_eq!(entry.active_streams, vec![16]);
        assert_eq!(
            entry.active_stream_expires,
            vec![custom_exp],
            "416 response must roll back active_stream_expires to prevent expiry extension DoS"
        );
    }

    // 2b. `404 NOT_FOUND` during continuation (source file temporarily missing) rolls back `active_streams` & `active_stream_expires`
    let rb_file = exports_dir.join("exp_r12_rollback.mp4");
    let rb_file_bak = exports_dir.join("exp_r12_rollback.mp4.bak");
    std::fs::rename(&rb_file, &rb_file_bak).unwrap();
    let r_rb_404 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_rb}/video.mp4"))
                .header(header::RANGE, "bytes=16-63")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_rb_404.status(), StatusCode::NOT_FOUND);
    {
        let map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get(&jti_rb).unwrap();
        assert_eq!(
            entry.active_streams,
            vec![16],
            "404 error in share_video_mp4 must roll back active_streams[0] to 16"
        );
        assert_eq!(
            entry.active_stream_expires,
            vec![custom_exp],
            "404 error in share_video_mp4 must roll back active_stream_expires[0]"
        );
    }
    std::fs::rename(&rb_file_bak, &rb_file).unwrap();

    // Retry the exact same `Range: bytes=16-63` continuation -> now succeeds with `206 PARTIAL_CONTENT`!
    let r_rb_retry = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_rb}/video.mp4"))
                .header(header::RANGE, "bytes=16-63")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_rb_retry.status(),
        StatusCode::PARTIAL_CONTENT,
        "legitimate Range: bytes=16-63 retry after transient 404 must succeed"
    );
    {
        let map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get(&jti_rb).unwrap();
        assert_eq!(entry.active_streams, vec![64]);
    }

    // 2c. When `!stream_active` in `record_share_access_or_gone`, only the matched cursor slot is removed
    //     (never blindly popping a concurrent viewer's cursor)
    {
        let mut map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get_mut(&jti_rb).unwrap();
        entry.active_streams = vec![64, 96];
        entry.active_stream_expires = vec![now + 900, now + 900];
        entry.sync_stream_summary();
    }
    let rb_payload = ShareTokenPayload {
        k: "export".to_string(),
        id: "exp_r12_rollback".to_string(),
        exp: now + 3600,
        cam: Some("front_door".to_string()),
        jti: Some(jti_rb.clone()),
    };
    // Unmatched final range (`tentative_next = 40`, `prev_stream_cursor = None`) does not pop `96` or `64`
    let _ = record_share_access_or_gone(
        &state,
        &rb_payload,
        false,
        false,
        Some((32, Some(39))),
        now,
        None,
        false,
        true,
        None,
        StatusCode::OK.into_response(),
    );
    {
        let map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get(&jti_rb).unwrap();
        assert_eq!(
            entry.active_streams,
            vec![64, 96],
            "unmatched !stream_active completion must not blindly pop a concurrent viewer's stream cursor"
        );
    }
    // Completing stream 0 (`Range: bytes=64-127` -> reaches EOF `127/128`, `!stream_active`) removes slot 0 (`64`), keeping `96`!
    let r_rb_eof = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_rb}/video.mp4"))
                .header(header::RANGE, "bytes=64-127")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_rb_eof.status(), StatusCode::PARTIAL_CONTENT);
    {
        let map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get(&jti_rb).unwrap();
        assert_eq!(
            entry.active_streams,
            vec![96],
            "completing stream slot 0 must remove only slot 0 and preserve concurrent stream slot 96"
        );
    }

    // =========================================================================
    // 3. [R12-FINDING-03] Camera Identifier Sanitation, Reserved-Prefix Guard,
    //    & Finite `[start, end]` Bounds Check in `api_export_create`
    // =========================================================================
    for bad_uri in [
        "/api/export/share_clip_front_door/start/6000/end/6010",
        "/api/export/share_redacted_front_door/start/6000/end/6010",
        "/api/export/.hidden_cam/start/6000/end/6010",
        "/api/export/front..door/start/6000/end/6010",
        "/api/export/front$door/start/6000/end/6010",
        "/api/export/unknown_camera/start/6000/end/6010",
        "/api/export/front_door/start/NaN/end/6010",
        "/api/export/front_door/start/6000/end/inf",
        "/api/export/front_door/start/-5/end/6010",
        "/api/export/front_door/start/6010/end/6010",
        "/api/export/front_door/start/6010/end/6000",
        "/api/export/front_door/start/1000/end/90000",
    ] {
        let r_bad_exp = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(bad_uri)
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_bad_exp.status(),
            StatusCode::BAD_REQUEST,
            "POST {bad_uri} must be rejected with 400 BAD_REQUEST"
        );
    }
    assert!(
        state
            .db
            .get_export_by_id("share_clip_front_door_6000_6010")
            .unwrap()
            .is_none(),
        "reserved prefix share_clip_* must never be inserted into export table"
    );
    assert!(
        state
            .db
            .get_export_by_id("front_door_0_6010")
            .unwrap()
            .is_none(),
        "NaN/negative start timestamp must never be cast to 0 and inserted into export table"
    );

    // =========================================================================
    // 4. [R12-FINDING-04] Refresh SQLite `last_accessed_at` via
    //    `touch_share_link_last_accessed` During Active `?sid=` / Continuation Playback
    // =========================================================================
    let (jti_t1, tok_t1) = mint_share("export", "exp_r12_touch1", Some(1)).await;
    let r_t1_page = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_t1}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_t1_page.status(), StatusCode::OK);
    let t1_html = String::from_utf8(
        to_bytes(r_t1_page.into_body(), 16384)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let sid_t1 = {
        let marker = format!("/share/{tok_t1}/video.mp4?sid=");
        let start_idx = t1_html.find(&marker).unwrap() + marker.len();
        t1_html[start_idx..start_idx + 32].to_string()
    };

    // Backdate SQLite `last_accessed_at` to `now - 1000` (outside 900s grace window)
    // and set in-memory session expiry to `now + 50`
    state
        .db
        .execute_batch(&format!(
            "UPDATE share_link SET last_accessed_at = {} WHERE id = '{jti_t1}';",
            (now - 1000) as f64
        ))
        .unwrap();
    {
        let mut map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get_mut(&jti_t1).unwrap();
        entry.sessions.get_mut(&sid_t1).unwrap().0 = now + 50;
    }
    assert!(
        !state
            .db
            .has_recently_accessed_share_links_for_target("exp_r12_touch1", now as f64, 900.0)
            .unwrap()
    );

    // 4a. Initial `?sid=` video claim refreshes BOTH in-memory session `*exp` AND SQLite `last_accessed_at`
    let r_t1_vid1 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_t1}/video.mp4?sid={sid_t1}"))
                .header(header::RANGE, "bytes=0-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_t1_vid1.status(), StatusCode::PARTIAL_CONTENT);
    {
        let map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get(&jti_t1).unwrap();
        let (sess_exp, claimed) = entry.sessions.get(&sid_t1).copied().unwrap();
        assert!(claimed);
        assert!(
            sess_exp >= now + 895,
            "?sid= claim must refresh session expiry to now_ts + 900"
        );
    }
    assert!(
        state
            .db
            .has_recently_accessed_share_links_for_target("exp_r12_touch1", now as f64, 900.0)
            .unwrap(),
        "?sid= video playback must refresh SQLite last_accessed_at via touch_share_link_last_accessed"
    );

    // 4b. Subsequent ranged `?sid=` chunk on already-claimed session also refreshes `*exp` & SQLite `last_accessed_at`
    state
        .db
        .execute_batch(&format!(
            "UPDATE share_link SET last_accessed_at = {} WHERE id = '{jti_t1}';",
            (now - 1000) as f64
        ))
        .unwrap();
    {
        let mut map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get_mut(&jti_t1).unwrap();
        entry.sessions.get_mut(&sid_t1).unwrap().0 = now + 50;
    }
    let r_t1_vid2 = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_t1}/video.mp4?sid={sid_t1}"))
                .header(header::RANGE, "bytes=16-63")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_t1_vid2.status(), StatusCode::PARTIAL_CONTENT);
    {
        let map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get(&jti_t1).unwrap();
        let (sess_exp, _) = entry.sessions.get(&sid_t1).copied().unwrap();
        assert!(sess_exp >= now + 895);
    }
    assert!(
        state
            .db
            .has_recently_accessed_share_links_for_target("exp_r12_touch1", now as f64, 900.0)
            .unwrap()
    );

    // 4c & 4d. `consumed_credit` (un-sid initial video after page load) & `valid_continuation` (un-sid range continuation)
    //          both refresh SQLite `last_accessed_at`
    let (jti_t2, tok_t2) = mint_share("export", "exp_r12_touch2", Some(1)).await;
    let r_t2_page = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_t2}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_t2_page.status(), StatusCode::OK);

    state
        .db
        .execute_batch(&format!(
            "UPDATE share_link SET last_accessed_at = {} WHERE id = '{jti_t2}';",
            (now - 1000) as f64
        ))
        .unwrap();
    let r_t2_init = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_t2}/video.mp4"))
                .header(header::RANGE, "bytes=0-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_t2_init.status(), StatusCode::PARTIAL_CONTENT);
    assert!(
        state
            .db
            .has_recently_accessed_share_links_for_target("exp_r12_touch2", now as f64, 900.0)
            .unwrap(),
        "consumed_credit branch must refresh SQLite last_accessed_at"
    );

    state
        .db
        .execute_batch(&format!(
            "UPDATE share_link SET last_accessed_at = {} WHERE id = '{jti_t2}';",
            (now - 1000) as f64
        ))
        .unwrap();
    let r_t2_cont = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/share/{tok_t2}/video.mp4"))
                .header(header::RANGE, "bytes=16-63")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_t2_cont.status(), StatusCode::PARTIAL_CONTENT);
    assert!(
        state
            .db
            .has_recently_accessed_share_links_for_target("exp_r12_touch2", now as f64, 900.0)
            .unwrap(),
        "valid_continuation branch must refresh SQLite last_accessed_at"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_round14_critical_audit_findings_regression_work() {
    let tmp_dir = std::env::temp_dir().join(format!("ninjanvr_sec_r14_{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
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
  proxy:
    enabled: true
    trusted_proxies:
      - "127.0.0.1/32"
      - "10.42.0.0/16"
genai:
  enabled: true
  provider: openai
  api_key: "sk-saved-secret-admin-key"
  api_key_env: "OPENAI_API_KEY"
  base_url: "https://saved-llm.example.invalid/v1"
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/front_door
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/backyard
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("scoped_fd", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("viewer_u", "pass1234", "viewer", "*", 1000)
        .unwrap();

    let auth_app = build_router_with_auth(state.clone(), true);
    let now = unix_now();
    let mint_jwt = |sub: &str, role: &str, cams: &str| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: Some(cams.to_string()),
                iat: unix_now(),
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };
    let admin_jwt = mint_jwt("admin", "admin", "*");
    let admin_bearer = format!("Bearer {admin_jwt}");
    let admin_cookie = format!("ninjanvr_token={admin_jwt}");
    let scoped_bearer = format!("Bearer {}", mint_jwt("scoped_fd", "operator", "front_door"));
    let viewer_bearer = format!("Bearer {}", mint_jwt("viewer_u", "viewer", "*"));
    let deleted_admin_bearer = format!("Bearer {}", mint_jwt("deleted_root_admin", "admin", "*"));

    // =========================================================================
    // 1. [R14-CRIT-01] GenAI Admin Guard, CSRF, Env-Var Allowlist, Key Theft & SSRF Prevention
    // =========================================================================
    for bearer in [&viewer_bearer, &scoped_bearer] {
        let r_get = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/genai/models?validate=true&base_url=http://127.0.0.1:9999")
                    .header(header::AUTHORIZATION, bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_get.status(), StatusCode::FORBIDDEN);

        let r_post = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/genai/models")
                    .header(header::AUTHORIZATION, bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"provider":"openai"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_post.status(), StatusCode::FORBIDDEN);
    }

    // GET /api/genai/models with query params returns [] without performing outbound SSRF
    let r_admin_get_models = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/genai/models?validate=true&base_url=http://169.254.169.254/latest")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_admin_get_models.status(), StatusCode::OK);
    let get_models_json: Value = serde_json::from_slice(
        &to_bytes(r_admin_get_models.into_body(), 4096)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(get_models_json, json!([]));

    // build_effective_genai_config prevents saved key theft on base_url change and blocks non-allowlisted env vars
    let saved_genai = state.config.read().await.genai.clone();
    let req_redirect = GenAiValidateRequest {
        provider: Some("openai".to_string()),
        base_url: Some("https://attacker.example.com/v1".to_string()),
        api_key: None,
        api_key_env: Some("NINJANVR_JWT_SECRET".to_string()),
        clear_api_key: None,
        validate: Some(true),
    };
    let eff_redirect = build_effective_genai_config(&saved_genai, &req_redirect);
    assert_eq!(eff_redirect.api_key, None);
    assert_eq!(eff_redirect.api_key_env, "");
    assert_eq!(eff_redirect.resolve_api_key(), "");

    // SSRF base_url validation rejects metadata IP, IPv6 link-local, embedded credentials, and internal ports
    for bad_base in [
        "http://169.254.169.254/latest/meta-data",
        "http://[fe80::1]:8080/v1",
        "https://metadata.google.internal/v1",
        "http://user:pass@example.com/v1",
        "http://127.0.0.1:5000/api",
        "http://127.0.0.1:1984/api",
        "file:///etc/passwd",
    ] {
        let r_ssrf = auth_app
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
                            "base_url": bad_base
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_ssrf.status(),
            StatusCode::BAD_REQUEST,
            "Disallowed GenAI base_url {bad_base} must be rejected with 400 BAD_REQUEST"
        );
    }

    // =========================================================================
    // 2. [R14-CRIT-02] Trusted-Proxy ForwardAuth Hardening & Loopback Spoofing Block
    // =========================================================================
    // Spoofed X-Real-Ip: 127.0.0.1 or X-Forwarded-For: 127.0.0.1 without x-peer-ip is rejected!
    for (hdr_name, hdr_val) in [
        ("x-real-ip", "127.0.0.1"),
        ("x-forwarded-for", "127.0.0.1"),
        ("x-real-ip", "::1"),
    ] {
        let r_spoof = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/config/raw")
                    .header("Remote-User", "admin")
                    .header("Remote-Role", "admin")
                    .header(hdr_name, hdr_val)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_spoof.status(),
            StatusCode::UNAUTHORIZED,
            "Spoofed {hdr_name}: {hdr_val} must not match default loopback trusted_proxies"
        );
    }

    // Untrusted x-peer-ip cannot be overridden by trusted x-real-ip
    let r_peer_mismatch = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .header("x-peer-ip", "203.0.113.99")
                .header("x-real-ip", "10.42.5.19")
                .header("Remote-User", "admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_peer_mismatch.status(), StatusCode::UNAUTHORIZED);

    // Unconfigured Remote-Role: admin is ignored when Remote-Groups has no admin group
    let r_role_spoof = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/users")
                .header("x-peer-ip", "127.0.0.1")
                .header("Remote-User", "proxy_low_priv")
                .header("Remote-Role", "admin")
                .header("Remote-Groups", "viewers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_role_spoof.status(),
        StatusCode::FORBIDDEN,
        "Unconfigured Remote-Role header must not escalate ForwardAuth user to admin"
    );

    // Deleted non-default admin JWT is rejected even if Remote-User header is present
    let r_del_admin = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/users")
                .header(header::AUTHORIZATION, &deleted_admin_bearer)
                .header("Remote-User", "admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_del_admin.status(), StatusCode::UNAUTHORIZED);

    // =========================================================================
    // 3. [R14-CRIT-03] Per-Camera RBAC, /ws Scoping, & Privacy Mask on Clips/Previews
    // =========================================================================
    let clips_dir = media_dir.join("clips");
    let rec_fd_dir = media_dir.join("recordings/2026-11-04/12/front_door");
    let rec_by_dir = media_dir.join("recordings/2026-11-04/12/backyard");
    std::fs::create_dir_all(&clips_dir).unwrap();
    std::fs::create_dir_all(&rec_fd_dir).unwrap();
    std::fs::create_dir_all(&rec_by_dir).unwrap();
    std::fs::write(rec_fd_dir.join("10.00.mp4"), b"FD_RAW_MP4_BYTES").unwrap();
    std::fs::write(rec_by_dir.join("10.00.mp4"), b"BY_RAW_MP4_BYTES").unwrap();

    for (cam, ev_id, rev_id, rec_id, path) in [
        (
            "front_door",
            "ev_r14_fd",
            "rev_r14_fd",
            "rec_r14_fd",
            "/media/ninjanvr/recordings/2026-11-04/12/front_door/10.00.mp4",
        ),
        (
            "backyard",
            "ev_r14_by",
            "rev_r14_by",
            "rec_r14_by",
            "/media/ninjanvr/recordings/2026-11-04/12/backyard/10.00.mp4",
        ),
    ] {
        state
            .db
            .insert_recording(&ninjanvr_db::RecordingRow {
                id: rec_id.to_string(),
                camera: cam.to_string(),
                path: path.to_string(),
                start_time: 7000.0,
                end_time: 7010.0,
                duration: 10.0,
                motion: 3,
                objects: 1,
                dbfs: -20,
                segment_size: 0.1,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: ev_id.to_string(),
                camera: cam.to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 7001.0,
                end_time: Some(7008.0),
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
        state
            .db
            .insert_review_segment(&ninjanvr_db::ReviewItemDto {
                id: rev_id.to_string(),
                camera: cam.to_string(),
                severity: "alert".to_string(),
                start_time: 7001.0,
                end_time: Some(7008.0),
                has_been_reviewed: false,
                data: json!({ "detections": [ev_id], "objects": ["person"] }),
                thumb_path: format!("/clips/review/thumb-{cam}-7001-{rev_id}.webp"),
            })
            .unwrap();
    }
    let _ =
        create_dataset_sample_from_event(&state.db, &media_dir, "ev_r14_fd", "true_positive", None)
            .await
            .unwrap();
    let _ =
        create_dataset_sample_from_event(&state.db, &media_dir, "ev_r14_by", "true_positive", None)
            .await
            .unwrap();

    for forbidden_uri in [
        "/live/mse/api/ws?src=backyard",
        "/live/webrtc/api/ws?src=backyard",
        "/live/mse/api/ws",
        "/live/mse/api/ws?src=front_door&src=backyard",
        "/api/go2rtc/webrtc?src=front_door&src=%62ackyard",
        "/api/go2rtc/streams/backyard",
        "/api/go2rtc/streams/backyard_sub/prewarm",
        "/api/events/ev_r14_by",
        "/api/events/ev_r14_by/thumbnail.webp",
        "/api/events/ev_r14_by/thumbnail.jpg",
        "/api/events/ev_r14_by/snapshot.jpg",
        "/api/events/ev_r14_by/snapshot-clean.webp",
        "/api/events/ev_r14_by/clip.mp4",
        "/api/review/rev_r14_by",
        "/api/review/rev_r14_by/preview",
        "/api/review/event/ev_r14_by",
        "/api/preview/backyard/start/0/end/99999",
        "/api/preview/preview_backyard-7000.mp4/thumbnail.webp",
        "/api/dataset/samples/ds_ev_r14_by",
        "/api/dataset/samples/ds_ev_r14_by/image.webp",
    ] {
        let method = if forbidden_uri.ends_with("/prewarm")
            || forbidden_uri.starts_with("/api/go2rtc/webrtc")
        {
            "POST"
        } else {
            "GET"
        };
        let r = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(forbidden_uri)
                    .header(header::AUTHORIZATION, &scoped_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::FORBIDDEN,
            "Camera-restricted user must receive 403 FORBIDDEN on {method} {forbidden_uri}"
        );
    }

    // filter_ws_message_for_allowed_cameras filters cross-camera WS broadcasts
    let allowed_only_fd = vec!["front_door".to_string()];
    assert!(
        filter_ws_message_for_allowed_cameras(
            &json!({"topic": "backyard/motion", "payload": "ON"}).to_string(),
            Some(&allowed_only_fd)
        )
        .is_none()
    );
    assert!(
        filter_ws_message_for_allowed_cameras(
            &json!({"topic": "front_door/motion", "payload": "ON"}).to_string(),
            Some(&allowed_only_fd)
        )
        .is_some()
    );
    let filtered_act = filter_ws_message_for_allowed_cameras(
            &json!({
                "topic": "camera_activity",
                "payload": json!({"front_door": {"motion": true}, "backyard": {"motion": true}}).to_string()
            })
            .to_string(),
            Some(&allowed_only_fd),
        )
        .unwrap();
    assert!(filtered_act.contains("front_door") && !filtered_act.contains("backyard"));

    // Privacy mask on front_door blocks raw unredacted segment leak on clip.mp4 and review preview
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask =
            json!([[[0.1, 0.1], [0.5, 0.1], [0.5, 0.5], [0.1, 0.5]]]);
    }
    for masked_uri in [
        "/api/events/ev_r14_fd/clip.mp4",
        "/api/review/rev_r14_fd/preview",
    ] {
        let r_masked = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(masked_uri)
                    .header(header::AUTHORIZATION, &scoped_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_masked.status(),
            StatusCode::NOT_FOUND,
            "{masked_uri} must not leak raw unredacted segment when privacy_mask is active"
        );
    }
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask = Value::Null;
    }

    // =========================================================================
    // 4. [R14-CRIT-04] Global CSRF Gate, Admin Guards on Upload/Probe/Federation, & Scoped Mutations
    // =========================================================================
    // Global CSRF gate in auth_gate_middleware blocks cross-origin cookie mutations
    for (method, uri, body) in [
        (
            "POST",
            "/api/users",
            r#"{"username":"csrf_u","password":"password123"}"#,
        ),
        (
            "POST",
            "/api/tokens",
            r#"{"name":"csrf_tok","scope":"full"}"#,
        ),
        ("POST", "/api/profile", r#"{"profile":"disarmed"}"#),
        ("POST", "/api/front_door/state", r#"{"record":false}"#),
        ("POST", "/api/front_door/ptz/stop", r#"{}"#),
    ] {
        let r_csrf = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::COOKIE, &admin_cookie)
                    .header(header::ORIGIN, "https://evil.example")
                    .header(header::HOST, "nvr.local:8971")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_csrf.status(),
            StatusCode::FORBIDDEN,
            "Global CSRF gate must reject cross-origin cookie {method} {uri}"
        );
    }

    // Camera-restricted operator is blocked from admin endpoints and cross-camera mutations
    for (method, uri, body) in [
        (
            "POST",
            "/api/detectors/models/upload",
            r#"{"filename":"evil.onnx"}"#,
        ),
        (
            "POST",
            "/api/streams/probe",
            r#"{"url":"rtsp://127.0.0.1:8554/front_door"}"#,
        ),
        ("GET", "/api/federation/nodes", ""),
        ("GET", "/api/federation/peer1/stats", ""),
        ("POST", "/api/debug/reprocess", r#"{}"#),
        ("POST", "/api/notifications/test", ""),
        ("POST", "/api/faces/Alice/enroll", ""),
        ("DELETE", "/api/faces/Alice", ""),
        ("POST", "/api/dataset/samples/from-event/ev_r14_fd", r#"{}"#),
        ("POST", "/api/profile", r#"{"profile":"disarmed"}"#),
        ("POST", "/api/events/ev_r14_by/retain", ""),
        (
            "PUT",
            "/api/events/ev_r14_by/false_positive",
            r#"{"false_positive":true}"#,
        ),
        ("POST", "/api/events/ev_r14_by/verify-ai", ""),
        (
            "POST",
            "/api/events/ev_r14_by/description",
            r#"{"description":"x"}"#,
        ),
        (
            "POST",
            "/api/reviews/viewed",
            r#"{"ids":["rev_r14_by"],"reviewed":true}"#,
        ),
    ] {
        let r_op = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::AUTHORIZATION, &scoped_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_op.status(),
            StatusCode::FORBIDDEN,
            "Scoped operator must receive 403 FORBIDDEN on {method} {uri}"
        );
    }

    // Detector model upload validates extensions and clamps dimensions to 64..=2048
    let r_bad_ext = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/detectors/models/upload")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"filename":"exploit.so"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_bad_ext.status(), StatusCode::BAD_REQUEST);

    let r_clamp = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/detectors/models/upload")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"filename":"clamped.onnx","width":65535,"height":8}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_clamp.status(), StatusCode::OK);
    let clamp_json: Value =
        serde_json::from_slice(&to_bytes(r_clamp.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(clamp_json["width"], 2048);
    assert_eq!(clamp_json["height"], 64);

    // Streams probe rejects non-rtsp schemes, local files, and CLI flags
    for bad_probe_url in [
        "file:///etc/passwd",
        "/etc/passwd",
        "http://169.254.169.254/latest/meta-data",
        "-version",
    ] {
        let r_probe = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/streams/probe")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(json!({ "url": bad_probe_url }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_probe.status(),
            StatusCode::BAD_REQUEST,
            "Disallowed probe URL {bad_probe_url} must return 400 BAD_REQUEST"
        );
    }

    // Federation proxy rejects path traversal / authority injection in subpath
    for bad_fed_uri in [
        "/api/federation/peer1/evil@attacker.example/config",
        "/api/federation/peer1/a..b/config",
    ] {
        let r_fed_bad = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(bad_fed_uri)
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_fed_bad.status(), StatusCode::BAD_REQUEST);
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_round16_critical_audit_findings_regression_work() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-r16-crit-audit-{}", rand::random::<u64>()));
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(&media_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html>Console v2</html>").unwrap();

    let cfg_path = tmp_dir.join("config.yml");
    let db_path = tmp_dir.join("ninjanvr.db");
    let yaml = format!(
        r#"
mqtt:
  enabled: false
  user: mqtt_admin
  password: "mqtt_super_secret_password"
database:
  path: {}
auth:
  enabled: true
genai:
  enabled: true
  provider: openai
  api_key: "sk-r16-super-secret-genai-key"
  base_url: "https://saved-llm.example.invalid/v1"
go2rtc:
  streams:
    front_door: "rtsp://camuser:campass123@192.0.2.10:554/main"
    front_door_sub: "rtsp://camuser:campass123@192.0.2.10:554/sub"
    backyard: "rtsp://byuser:bypass456@192.0.2.20:554/main"
    backyard_sub: "rtsp://byuser:bypass456@192.0.2.20:554/sub"
camera_groups:
  mixed_group:
    order: 1
    cameras: [front_door, backyard]
  forbidden_group:
    order: 2
    cameras: [backyard]
cameras:
  front_door:
    enabled: true
    webui_url: "http://192.0.2.10:1"
    onvif:
      host: "http://onvifuser:onvifpass789@192.0.2.10:1"
      port: 1
      user: onvifuser
      password: "onvif_secret_password"
      timeout_sec: 1
    ffmpeg:
      inputs:
        - path: rtsp://camuser:campass123@192.0.2.10:554/main
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://byuser:bypass456@192.0.2.20:554/main
          roles: [record, detect]
  front-door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://192.0.2.30:554/main
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("scoped_fd", "pass1234", "operator", "front_door", 1000)
        .unwrap();

    let auth_app = build_router_with_auth(state.clone(), true);
    let now = unix_now();
    let mint_jwt = |sub: &str, role: &str, cams: &str| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: Some(cams.to_string()),
                iat: unix_now(),
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };
    let admin_bearer = format!("Bearer {}", mint_jwt("admin", "admin", "*"));
    let scoped_bearer = format!("Bearer {}", mint_jwt("scoped_fd", "operator", "front_door"));

    // =========================================================================
    // 1. [R16-CRIT-01] Config/ONVIF Scrubbing, Save Restore, Scoped Config, & Camera Proxy
    // =========================================================================
    let r_raw = auth_app
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
    let raw_yaml_str: String =
        serde_json::from_slice(&to_bytes(r_raw.into_body(), 65536).await.unwrap()).unwrap();
    for secret in [
        "campass123",
        "bypass456",
        "mqtt_super_secret_password",
        "onvif_secret_password",
        "onvifpass789",
        "sk-r16-super-secret-genai-key",
    ] {
        assert!(
            !raw_yaml_str.contains(secret),
            "GET /api/config/raw must scrub secret '{secret}'"
        );
    }

    // Saving the scrubbed YAML via POST /api/config/save restores secrets on disk
    let r_save = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/save")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::from(raw_yaml_str.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_save.status(), StatusCode::OK);
    let saved_disk = std::fs::read_to_string(&cfg_path).unwrap();
    for secret in [
        "campass123",
        "bypass456",
        "mqtt_super_secret_password",
        "onvif_secret_password",
        "sk-r16-super-secret-genai-key",
    ] {
        assert!(
            saved_disk.contains(secret),
            "POST /api/config/save must restore secret '{secret}' on disk instead of clobbering"
        );
    }

    // POST /api/config/structured returns scrubbed raw_yaml while keeping secrets on disk
    let r_struct = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/structured")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "ui": {
                            "timezone": "UTC"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_struct.status(), StatusCode::OK);
    let struct_json: Value =
        serde_json::from_slice(&to_bytes(r_struct.into_body(), 65536).await.unwrap()).unwrap();
    let struct_raw_yaml = struct_json["raw_yaml"].as_str().unwrap();
    assert!(!struct_raw_yaml.contains("campass123"));
    assert!(!struct_raw_yaml.contains("sk-r16-super-secret-genai-key"));
    let disk_after_struct = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(disk_after_struct.contains("campass123"));
    assert!(disk_after_struct.contains("sk-r16-super-secret-genai-key"));

    // Scoped GET /api/config filters cameras, go2rtc.streams, and camera_groups
    let r_cfg_scoped = auth_app
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
    let cfg_scoped_json: Value =
        serde_json::from_slice(&to_bytes(r_cfg_scoped.into_body(), 65536).await.unwrap()).unwrap();
    assert!(cfg_scoped_json["cameras"].get("front_door").is_some());
    assert!(cfg_scoped_json["cameras"].get("backyard").is_none());
    let go2rtc_streams = cfg_scoped_json["go2rtc"]["streams"].as_object().unwrap();
    assert!(go2rtc_streams.contains_key("front_door"));
    assert!(go2rtc_streams.contains_key("front_door_sub"));
    assert!(!go2rtc_streams.contains_key("backyard"));
    assert!(!go2rtc_streams.contains_key("backyard_sub"));
    let cam_groups = cfg_scoped_json["camera_groups"].as_object().unwrap();
    assert_eq!(cam_groups["mixed_group"]["cameras"], json!(["front_door"]));
    assert!(!cam_groups.contains_key("forbidden_group"));

    // ONVIF ptz/info scrubs onvif.host and error messages
    let r_ptz_info = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/ptz/info?timeout_ms=25")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ptz_info.status(), StatusCode::OK);
    let ptz_info_bytes = to_bytes(r_ptz_info.into_body(), 16384).await.unwrap();
    let ptz_info_str = std::str::from_utf8(&ptz_info_bytes).unwrap();
    assert!(
        !ptz_info_str.contains("onvifpass789"),
        "ptz/info must scrub credentials in onvif.host and SOAP error"
    );

    // Camera WebUI proxy rejects traversal/authority injection in subpath with 400 BAD_REQUEST
    for bad_proxy_uri in [
        "/api/camera-proxy/front_door/evil@attacker.example/login",
        "/api/camera-proxy/front_door/a..b/index.html",
    ] {
        let r_proxy_bad = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(bad_proxy_uri)
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_proxy_bad.status(), StatusCode::BAD_REQUEST);
    }
    let r_proxy_direct_bad =
        handle_camera_webui_proxy(&state, "front_door", "../admin", None).await;
    assert_eq!(r_proxy_direct_bad.status(), StatusCode::BAD_REQUEST);

    // =========================================================================
    // 2. [R16-CRIT-02] Canonical Path Normalization & Static Media Camera Attribution
    // =========================================================================
    assert_eq!(
        normalize_http_path_for_auth("/api//front_door/%72ecordings/"),
        Some("/api/front_door/recordings".to_string())
    );
    assert_eq!(normalize_http_path_for_auth("/api/../users"), None);
    assert_eq!(normalize_http_path_for_auth("/api/%2e%2e/users"), None);

    // Percent-encoded, trailing-slash, and double-slash admin-only bypasses are blocked for non-admin
    for (method, bypass_uri) in [
        ("DELETE", "/api/front_door/%72ecordings"),
        ("DELETE", "/api/front_door/recordings/"),
        ("GET", "/api/%63onfig/raw"),
        ("GET", "/api//config/raw"),
        ("GET", "/vod//backyard/start/0/end/10/master.m3u8"),
        ("GET", "/vod/clip//backyard/start/0/end/10/master.m3u8"),
        ("GET", "/vod/"),
    ] {
        let r_bypass = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(bypass_uri)
                    .header(header::AUTHORIZATION, &scoped_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_bypass.status(),
            StatusCode::FORBIDDEN,
            "Path normalization must block {method} {bypass_uri} with 403 FORBIDDEN"
        );
    }

    // Static media attribution: -clean snapshots, spoofed event ID prefix, latest_{camera}.jpg, review thumbs
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "front_door-spoof_by".to_string(),
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
    assert_eq!(
        resolve_static_media_camera(&state, "/clips/front_door-spoof_by-clean.jpg").as_deref(),
        Some("backyard")
    );
    assert_eq!(
        resolve_static_media_camera(&state, "/clips/front_door-front_door-spoof_by-clean.webp")
            .as_deref(),
        Some("backyard")
    );
    assert_eq!(
        resolve_static_media_camera(&state, "/clips/front-door-unseeded_ev-clean.webp").as_deref(),
        Some("front-door")
    );
    assert_eq!(
        resolve_static_media_camera(&state, "/clips/latest_backyard.jpg").as_deref(),
        Some("backyard")
    );
    assert_eq!(
        resolve_static_media_camera(&state, "/clips/review/thumb-front-door-rev1.webp").as_deref(),
        Some("front-door")
    );
    assert_eq!(
        resolve_static_media_camera(&state, "/clips/review/thumb-front_door-1700-abc-1.webp")
            .as_deref(),
        Some("front_door")
    );

    // =========================================================================
    // 3. [R16-CRIT-03] VOD Handler Guards, ensure_path_within_media_dir, & Privacy Mask on latest.jpg
    // =========================================================================
    // Recording row pointing outside media_dir is rejected by existing_recordings_for_vod
    let outside_mp4 = tmp_dir.join("outside_secret.mp4");
    std::fs::write(&outside_mp4, b"OUTSIDE_MP4_DATA").unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_outside_r16".to_string(),
            camera: "front_door".to_string(),
            path: outside_mp4.to_string_lossy().to_string(),
            start_time: 9000.0,
            end_time: 9010.0,
            duration: 10.0,
            motion: 1,
            objects: 1,
            dbfs: -20,
            segment_size: 0.1,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
    assert!(existing_recordings_for_vod(&state, "front_door", 8990.0, 9020.0).is_empty());

    // Invalid camera ID on /api/{camera}/latest.jpg returns 400 BAD_REQUEST
    let r_bad_cam_latest = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front..door/latest.jpg")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_bad_cam_latest.status(), StatusCode::BAD_REQUEST);

    // Privacy mask is enforced on state.latest_frames AND clips/latest_{camera}.jpg
    let mut white_img = image::RgbImage::new(64, 64);
    for p in white_img.pixels_mut() {
        *p = image::Rgb([250, 250, 250]);
    }
    let white_jpeg = ninjanvr_vision::encode_image_jpeg(&white_img).unwrap();
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask =
            json!(["0.0,0.0,1.0,0.0,1.0,1.0,0.0,1.0"]);
    }
    state
        .latest_frames
        .write()
        .await
        .insert("front_door".to_string(), white_jpeg.clone());
    let r_masked_mem = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/latest.jpg")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_masked_mem.status(), StatusCode::OK);
    let masked_mem_bytes = to_bytes(r_masked_mem.into_body(), 65536).await.unwrap();
    let masked_mem_img = image::load_from_memory(&masked_mem_bytes)
        .unwrap()
        .to_rgb8();
    assert!(
        masked_mem_img.get_pixel(32, 32).0[0] < 30,
        "state.latest_frames must apply privacy_mask before returning latest.jpg"
    );

    state.latest_frames.write().await.remove("front_door");
    let clips_dir = media_dir.join("clips");
    std::fs::create_dir_all(&clips_dir).unwrap();
    std::fs::write(clips_dir.join("latest_front_door.jpg"), &white_jpeg).unwrap();
    let r_masked_disk = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/latest.jpg")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_masked_disk.status(), StatusCode::OK);
    let masked_disk_bytes = to_bytes(r_masked_disk.into_body(), 65536).await.unwrap();
    let masked_disk_img = image::load_from_memory(&masked_disk_bytes)
        .unwrap()
        .to_rgb8();
    assert!(
        masked_disk_img.get_pixel(32, 32).0[0] < 30,
        "clips/latest_front_door.jpg must apply privacy_mask before returning latest.jpg"
    );

    // =========================================================================
    // 4. [R16-CRIT-04] MCP set_security_profile, /api/cases, FP Signatures, & Telemetry Scoping
    // =========================================================================
    // Scoped operator cannot globally disarm NVR via MCP set_security_profile
    let r_mcp_disarm = auth_app
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
                            "name": "set_security_profile",
                            "arguments": { "profile": "disarmed" }
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_mcp_disarm.status(), StatusCode::FORBIDDEN);

    // Cases: admin creates case referencing backyard event; scoped user cannot read or overwrite it
    state
        .db
        .create_case(
            "case_admin_by",
            "Backyard Investigation",
            Some("Confidential backyard case"),
            &["front_door-spoof_by".to_string()],
            &[],
        )
        .unwrap();
    let r_cases_scoped = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/cases")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_cases_scoped.status(), StatusCode::OK);
    let cases_arr: Vec<Value> =
        serde_json::from_slice(&to_bytes(r_cases_scoped.into_body(), 16384).await.unwrap())
            .unwrap();
    assert!(
        cases_arr.iter().all(|c| c["id"] != "case_admin_by"),
        "Scoped user must not see cases referencing forbidden camera events"
    );

    // Scoped user cannot overwrite case_admin_by or attach forbidden event IDs
    for bad_case_payload in [
        json!({
            "id": "case_admin_by",
            "name": "Clobbered",
            "event_ids": []
        }),
        json!({
            "id": "case_scoped_spoof",
            "name": "Forbidden Event Ref",
            "event_ids": ["front_door-spoof_by"]
        }),
    ] {
        let r_case_forbid = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/cases")
                    .header(header::AUTHORIZATION, &scoped_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(bad_case_payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_case_forbid.status(), StatusCode::FORBIDDEN);
    }

    // FP signatures, stats, stats/history, recordings/summary, and recordings/storage are scoped
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_fp_fd_r16".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 8000.0,
            end_time: Some(8005.0),
            has_clip: false,
            has_snapshot: false,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.8),
            false_positive: Some(false),
            box_coords: Some(json!([0.1, 0.1, 0.2, 0.2])),
            data: json!({"box": [0.1, 0.1, 0.2, 0.2]}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.8),
            stationary: false,
        })
        .unwrap();
    state
        .db
        .set_event_false_positive("ev_fp_fd_r16", true)
        .unwrap();
    state
        .db
        .set_event_false_positive("front_door-spoof_by", true)
        .unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "rec_by_r16".to_string(),
            camera: "backyard".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-04/12/backyard/10.00.mp4".to_string(),
            start_time: 8000.0,
            end_time: 8010.0,
            duration: 10.0,
            motion: 2,
            objects: 1,
            dbfs: -20,
            segment_size: 1.5,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

    let r_fp = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/false_positives/signatures")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let fp_arr: Vec<Value> =
        serde_json::from_slice(&to_bytes(r_fp.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(fp_arr.len(), 1);
    assert_eq!(fp_arr[0]["camera"], "front_door");

    let r_stats = auth_app
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
    let stats_json: Value =
        serde_json::from_slice(&to_bytes(r_stats.into_body(), 65536).await.unwrap()).unwrap();
    assert!(stats_json["cameras"].get("front_door").is_some());
    assert!(stats_json["cameras"].get("backyard").is_none());
    assert!(
        stats_json["service"]["camera_storage"]
            .get("backyard")
            .is_none()
    );

    let r_rec_sum = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/recordings/summary")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let rec_sum_json: Value =
        serde_json::from_slice(&to_bytes(r_rec_sum.into_body(), 16384).await.unwrap()).unwrap();
    assert!(rec_sum_json.get("backyard").is_none());

    let r_rec_stor = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/recordings/storage")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let rec_stor_json: Value =
        serde_json::from_slice(&to_bytes(r_rec_stor.into_body(), 16384).await.unwrap()).unwrap();
    assert!(rec_stor_json.get("backyard").is_none());

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_round18_critical_audit_findings_regression_work() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-r18-crit-audit-{}", rand::random::<u64>()));
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(&media_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html>Console v2</html>").unwrap();

    let cfg_path = tmp_dir.join("config.yml");
    let db_path = tmp_dir.join("ninjanvr.db");
    let yaml = format!(
        r#"
mqtt:
  enabled: false
database:
  path: {}
auth:
  enabled: true
go2rtc:
  streams:
    front_door: "rtsp://camuser:campass123@192.0.2.10:554/main"
    front_door_sub: "rtsp://camuser:campass123@192.0.2.10:554/sub"
    backyard: "rtsp://byuser:bypass456@192.0.2.20:554/main"
    backyard_sub: "rtsp://byuser:bypass456@192.0.2.20:554/sub"
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://camuser:campass123@192.0.2.10:554/main
          roles: [record, detect]
  backyard:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://byuser:bypass456@192.0.2.20:554/main
          roles: [record, detect]
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("scoped_fd", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("unrestricted_viewer", "pass1234", "viewer", "*", 1000)
        .unwrap();

    let auth_app = build_router_with_auth(state.clone(), true);
    let unauth_app = build_router(state.clone());
    let now = unix_now();
    let mint_jwt = |sub: &str, role: &str, cams: Option<&str>| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: cams.map(str::to_string),
                iat: unix_now(),
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };
    let admin_bearer = format!("Bearer {}", mint_jwt("admin", "admin", Some("*")));
    let viewer_bearer = format!(
        "Bearer {}",
        mint_jwt("unrestricted_viewer", "viewer", Some("*"))
    );
    let scoped_bearer = format!(
        "Bearer {}",
        mint_jwt("scoped_fd", "operator", Some("front_door"))
    );
    let (_ro_meta, ro_tok) = state
        .db
        .create_api_token("ro_bot", "read_only", None)
        .unwrap();
    let ro_bearer = format!("Bearer {ro_tok}");

    // =========================================================================
    // 1. [R18-CRIT-01] Block Inline Producer RCE/SSRF & Raw WS Tunneling for All Callers
    // =========================================================================
    assert!(is_safe_go2rtc_stream_name("front_door"));
    assert!(is_safe_go2rtc_stream_name("front_door_sub"));
    assert!(is_safe_go2rtc_stream_name("cam-01"));
    for bad_stream in [
        "",
        ".hidden",
        "exec:id",
        "echo:hello",
        "expr:1+1",
        "ffmpeg:rtsp://127.0.0.1:8554/cam",
        "http://169.254.169.254/latest/meta-data",
        "rtsp://127.0.0.1:8554/front_door",
        "file:///etc/passwd",
        "front.door",
        "front/door",
        "front\\door",
        "front?door",
        "front#door",
        "front door",
        "front\0door",
        &"a".repeat(129),
    ] {
        assert!(
            !is_safe_go2rtc_stream_name(bad_stream),
            "is_safe_go2rtc_stream_name must reject '{bad_stream}'"
        );
    }

    assert_eq!(
        validate_go2rtc_query_streams(Some("src=front_door&status=201"), None),
        Ok(vec!["front_door".to_string()])
    );
    assert!(validate_go2rtc_query_streams(None, None).is_err());
    assert!(validate_go2rtc_query_streams(Some(""), None).is_err());
    assert!(validate_go2rtc_query_streams(Some("video=h264"), None).is_err());
    assert!(validate_go2rtc_query_streams(Some("src=exec:id"), None).is_err());
    assert!(validate_go2rtc_query_streams(Some("%73rc=exec%3Aid"), None).is_err());
    assert!(validate_go2rtc_query_streams(Some("dst=echo:test"), None).is_err());
    assert!(
        validate_go2rtc_query_streams(Some("src=backyard"), Some(&["front_door".to_string()]))
            .is_err()
    );

    // Even admin, unrestricted viewer, and read_only API tokens are blocked from inline producers & missing src
    for (method, bad_go2rtc_uri) in [
        ("GET", "/api/go2rtc/streams/exec:id"),
        ("GET", "/api/go2rtc/streams/echo%3Ahello"),
        ("GET", "/api/go2rtc/streams/front.door"),
        ("POST", "/api/go2rtc/streams/exec:id/prewarm"),
        ("POST", "/api/go2rtc/streams/ffmpeg%3Afile/prewarm"),
        ("POST", "/api/go2rtc/webrtc"),
        ("POST", "/api/go2rtc/webrtc?video=all"),
        ("POST", "/api/go2rtc/webrtc?src=exec:id"),
        ("POST", "/api/go2rtc/webrtc?src=front_door&dst=exec:id"),
        ("GET", "/live/mse/api/ws"),
        ("GET", "/live/mse/api/ws?video=h264"),
        ("GET", "/live/mse/api/ws?src=exec:id"),
        ("GET", "/live/mse/api/ws?%73rc=exec%3Aid"),
        ("GET", "/live/webrtc/api/ws?src=rtsp://127.0.0.1:8554/main"),
    ] {
        for bearer in [&admin_bearer, &viewer_bearer, &ro_bearer, &scoped_bearer] {
            let r = auth_app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(bad_go2rtc_uri)
                        .header(header::AUTHORIZATION, bearer)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                r.status() == StatusCode::BAD_REQUEST || r.status() == StatusCode::FORBIDDEN,
                "{method} {bad_go2rtc_uri} with bearer {bearer} must return 400 or 403, got {}",
                r.status()
            );
        }
        // Also verify direct handler enforcement on unauth_app
        let r_unauth = unauth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(bad_go2rtc_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_unauth.status(),
            StatusCode::BAD_REQUEST,
            "Direct handler {method} {bad_go2rtc_uri} must return 400 BAD_REQUEST"
        );
    }

    // =========================================================================
    // 2. [R18-CRIT-02] Defense-in-Depth Validation of Configured labelmap_path
    // =========================================================================
    assert!(is_safe_labelmap_path_str(
        "/openvino-model/coco_91cl_bkgr.txt"
    ));
    assert!(is_safe_labelmap_path_str(
        "models/ninjanvr_security_15cl.txt"
    ));
    for bad_lbl_path in [
        "",
        "/config/.jwt_secret",
        ".jwt_secret",
        "/config/config.yml",
        "../coco.txt",
        "/openvino-model/../.jwt_secret.txt",
        "/proc/self/environ",
        "/sys/kernel/uevent_helper",
        "/etc/passwd",
        "/dev/zero",
        "/run/secrets/token.txt",
    ] {
        assert!(
            !is_safe_labelmap_path_str(bad_lbl_path),
            "is_safe_labelmap_path_str must reject '{bad_lbl_path}'"
        );
    }

    for bad_save_yaml in [
        format!(
            "database:\n  path: {}\nmodel:\n  labelmap_path: /config/.jwt_secret\ncameras: {{}}\n",
            db_path.display()
        ),
        format!(
            "database:\n  path: {}\nmodel:\n  labelmap_path: /etc/passwd\ncameras: {{}}\n",
            db_path.display()
        ),
        format!(
            "database:\n  path: {}\nmodel:\n  labelmap_path: ../labels.txt\ncameras: {{}}\n",
            db_path.display()
        ),
        format!(
            "database:\n  path: {}\ndetectors:\n  ov:\n    type: openvino\n    model:\n      labelmap_path: .jwt_secret\ncameras: {{}}\n",
            db_path.display()
        ),
    ] {
        let r_bad_save = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/config/save")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::from(bad_save_yaml))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_bad_save.status(), StatusCode::BAD_REQUEST);
    }

    for bad_struct_json in [
        json!({ "model": { "labelmap_path": "/config/.jwt_secret" } }),
        json!({ "model": { "labelmap_path": "/proc/self/environ" } }),
        json!({ "model": { "labelmap_path": "../secret.txt" } }),
        json!({ "detectors": { "cpu1": { "type": "cpu", "model": { "labelmap_path": ".jwt_secret" } } } }),
    ] {
        let r_bad_struct = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/config/structured")
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(bad_struct_json.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_bad_struct.status(), StatusCode::BAD_REQUEST);
    }

    // Even if model.labelmap_path in memory points to .jwt_secret, GET /api/config never leaks .jwt_secret
    let jwt_secret_file = tmp_dir.join(".jwt_secret");
    {
        let mut cfg = state.config.write().await;
        cfg.model.labelmap_path = jwt_secret_file.to_string_lossy().to_string();
    }
    let r_cfg_safe = auth_app
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
    assert_eq!(r_cfg_safe.status(), StatusCode::OK);
    let cfg_safe_str = String::from_utf8(
        to_bytes(r_cfg_safe.into_body(), 65536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        !cfg_safe_str.contains(&state.jwt_secret),
        "GET /api/config must never leak .jwt_secret via model.labelmap"
    );
    {
        let mut cfg = state.config.write().await;
        cfg.model.labelmap_path = "/openvino-model/coco_91cl_bkgr.txt".to_string();
    }

    // =========================================================================
    // 3. [R18-CRIT-03] OIDC Callback Status/Claim Verification & resolve_external_base_url
    // =========================================================================
    // 3a. resolve_external_base_url blocks X-Forwarded-Host spoofing when Host is external
    let uri_login: Uri = "/api/auth/oidc/login".parse().unwrap();
    let mut spoof_hdrs = HeaderMap::new();
    spoof_hdrs.insert(header::HOST, "nvr.local:8971".parse().unwrap());
    spoof_hdrs.insert("x-forwarded-host", "evil.example".parse().unwrap());
    spoof_hdrs.insert("x-forwarded-proto", "https".parse().unwrap());
    assert_eq!(
        resolve_external_base_url(&spoof_hdrs, &uri_login),
        "https://nvr.local:8971"
    );

    // Honors X-Forwarded-Host when Host is internal loopback
    let mut proxy_hdrs = HeaderMap::new();
    proxy_hdrs.insert(header::HOST, "127.0.0.1:5000".parse().unwrap());
    proxy_hdrs.insert("x-forwarded-host", "nvr.example.invalid".parse().unwrap());
    proxy_hdrs.insert("x-forwarded-proto", "https".parse().unwrap());
    assert_eq!(
        resolve_external_base_url(&proxy_hdrs, &uri_login),
        "https://nvr.example.invalid"
    );

    // Sanitizes dangerous characters in Host / X-Forwarded-Proto
    let mut bad_host_hdrs = HeaderMap::new();
    bad_host_hdrs.insert("x-forwarded-host", "evil.example/path@foo".parse().unwrap());
    bad_host_hdrs.insert("x-forwarded-proto", "javascript".parse().unwrap());
    assert_eq!(
        resolve_external_base_url(&bad_host_hdrs, &uri_login),
        "http://localhost:5000"
    );

    // 3b. Mock OIDC IdP testing api_auth_oidc_callback
    let oidc_mode = Arc::new(std::sync::Mutex::new("tok_400".to_string()));
    let oidc_mode_tok = oidc_mode.clone();
    let oidc_mode_ui = oidc_mode.clone();
    let mock_idp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_idp_addr = mock_idp_listener.local_addr().unwrap();
    let issuer_url = format!("http://{mock_idp_addr}");
    let issuer_for_tok = issuer_url.clone();

    let issuer_for_disc = issuer_url.clone();
    let jwk_secret = b"oidc-r18-jwks-oct-signing-key-32";
    let jwk_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(jwk_secret);

    let make_b64_jwt = |claims: &Value, sig: &str| -> String {
        if sig.is_empty() {
            let hdr = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(r#"{"alg":"RS256","typ":"JWT"}"#);
            let pay = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(claims.to_string().as_bytes());
            format!("{hdr}.{pay}.")
        } else {
            let mut hdr = Header::new(Algorithm::HS256);
            hdr.kid = Some("k1".to_string());
            jsonwebtoken::encode(&hdr, claims, &EncodingKey::from_secret(jwk_secret)).unwrap()
        }
    };

    let mock_idp = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || {
                let iss = issuer_for_disc.clone();
                async move {
                    Json(json!({
                        "issuer": iss,
                        "token_endpoint": format!("{iss}/oauth/v2/token"),
                        "userinfo_endpoint": format!("{iss}/oidc/v1/userinfo"),
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
                    let iss = issuer_for_tok.clone();
                    let exp = unix_now() + 3600;
                    async move {
                        match mode.as_str() {
                            "tok_400" => (
                                StatusCode::BAD_REQUEST,
                                Json(json!({
                                    "id_token": make_b64_jwt(&json!({"sub":"admin","iss":iss,"aud":"ninjanvr-client","exp":exp}), "sig"),
                                    "access_token": "tok_bad"
                                })),
                            )
                                .into_response(),
                            "tok_error_field" => (
                                StatusCode::OK,
                                Json(json!({
                                    "error": "invalid_grant",
                                    "id_token": make_b64_jwt(&json!({"sub":"admin","iss":iss,"aud":"ninjanvr-client","exp":exp}), "sig")
                                })),
                            )
                                .into_response(),
                            "unsigned_id_token" => (
                                StatusCode::OK,
                                Json(json!({
                                    "id_token": make_b64_jwt(&json!({"sub":"admin","iss":iss,"aud":"ninjanvr-client","exp":exp}), "")
                                })),
                            )
                                .into_response(),
                            "bad_iss" => (
                                StatusCode::OK,
                                Json(json!({
                                    "id_token": make_b64_jwt(&json!({"sub":"admin","iss":"https://evil.example","aud":"ninjanvr-client","exp":exp}), "sig")
                                })),
                            )
                                .into_response(),
                            "bad_aud" => (
                                StatusCode::OK,
                                Json(json!({
                                    "id_token": make_b64_jwt(&json!({"sub":"admin","iss":iss,"aud":"other-client","exp":exp}), "sig")
                                })),
                            )
                                .into_response(),
                            "ui_401" | "sub_mismatch" | "no_identity" | "valid" => {
                                let id_claims = if mode == "no_identity" {
                                    json!({"iss":iss,"aud":"ninjanvr-client","exp":exp})
                                } else {
                                    json!({"sub":"alice","iss":iss,"aud":["ninjanvr-client"],"exp":exp,"groups":["admins"]})
                                };
                                (
                                    StatusCode::OK,
                                    Json(json!({
                                        "id_token": make_b64_jwt(&id_claims, "sig"),
                                        "access_token": "valid_access_tok"
                                    })),
                                )
                                    .into_response()
                            }
                            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                        }
                    }
                }),
            )
            .route(
                "/oidc/v1/userinfo",
                get(move || {
                    let mode = oidc_mode_ui.lock().unwrap().clone();
                    async move {
                        match mode.as_str() {
                            "ui_401" => (
                                StatusCode::UNAUTHORIZED,
                                Json(json!({ "error": "invalid_token" })),
                            )
                                .into_response(),
                            "sub_mismatch" => (
                                StatusCode::OK,
                                Json(json!({ "sub": "bob", "preferred_username": "bob" })),
                            )
                                .into_response(),
                            "no_identity" => (
                                StatusCode::OK,
                                Json(json!({ "groups": ["admins"] })),
                            )
                                .into_response(),
                            "valid" => (
                                StatusCode::OK,
                                Json(json!({
                                    "sub": "alice",
                                    "preferred_username": "alice_oidc",
                                    "groups": ["admins"]
                                })),
                            )
                                .into_response(),
                            _ => StatusCode::UNAUTHORIZED.into_response(),
                        }
                    }
                }),
            );
    let idp_handle = tokio::spawn(async move {
        let _ = axum::serve(mock_idp_listener, mock_idp).await;
    });

    {
        let mut cfg = state.config.write().await;
        cfg.auth.oidc.enabled = true;
        cfg.auth.oidc.issuer_url = issuer_url.clone();
        cfg.auth.oidc.client_id = "ninjanvr-client".to_string();
    }

    let signed_pkce_cookie = format!(
        "ninjanvr_oidc_pkce={}",
        format_signed_oidc_pkce_cookie(
            &state.jwt_secret,
            "state123",
            "verifier123",
            "http://localhost:5000/api/auth/oidc/callback",
        )
    );
    for rejected_mode in [
        "tok_400",
        "tok_error_field",
        "unsigned_id_token",
        "bad_iss",
        "bad_aud",
        "ui_401",
        "sub_mismatch",
        "no_identity",
    ] {
        *oidc_mode.lock().unwrap() = rejected_mode.to_string();
        let r_cb = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/oidc/callback?code=test_code&state=state123")
                    .header(header::COOKIE, &signed_pkce_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_cb.status(),
            StatusCode::UNAUTHORIZED,
            "OIDC callback mode '{rejected_mode}' must be rejected with 401 UNAUTHORIZED"
        );
        assert!(
            r_cb.headers().get(header::SET_COOKIE).is_none(),
            "OIDC callback mode '{rejected_mode}' must never issue a session cookie"
        );
    }

    *oidc_mode.lock().unwrap() = "valid".to_string();
    let r_cb_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/oidc/callback?code=test_code&state=state123")
                .header(header::COOKIE, &signed_pkce_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_cb_ok.status(), StatusCode::SEE_OTHER);
    assert!(
        r_cb_ok
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|v| v.to_str().unwrap_or("").contains("ninjanvr_token=")),
        "Valid OIDC callback must issue ninjanvr_token session cookie"
    );

    idp_handle.abort();
    {
        let mut cfg = state.config.write().await;
        cfg.auth.oidc.enabled = false;
        cfg.auth.oidc.issuer_url.clear();
    }

    // =========================================================================
    // 4. [R18-CRIT-04] Camera Scoping in /api/profile, /api/metrics, & /api/dataset/summary
    // =========================================================================
    // 4a. GET /api/profile for deleted local user (OIDC disabled) returns allowed_cameras: []
    let ghost_bearer = format!(
        "Bearer {}",
        mint_jwt("deleted_local_user", "operator", Some("*"))
    );
    let r_prof_ghost = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/profile")
                .header(header::AUTHORIZATION, &ghost_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_prof_ghost.status(), StatusCode::OK);
    let prof_ghost_json: Value =
        serde_json::from_slice(&to_bytes(r_prof_ghost.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(prof_ghost_json["allowed_cameras"], json!([]));

    // GET /api/profile for non-DB OIDC user with scoped JWT claim returns only ["front_door"]
    {
        let mut cfg = state.config.write().await;
        cfg.auth.oidc.enabled = true;
        cfg.auth.oidc.issuer_url = "https://idp.example.invalid".to_string();
    }
    let oidc_scoped_bearer = format!(
        "Bearer {}",
        mint_jwt("oidc_viewer_fd", "viewer", Some("front_door"))
    );
    let r_prof_oidc = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/profile")
                .header(header::AUTHORIZATION, &oidc_scoped_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_prof_oidc.status(), StatusCode::OK);
    let prof_oidc_json: Value =
        serde_json::from_slice(&to_bytes(r_prof_oidc.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(prof_oidc_json["allowed_cameras"], json!(["front_door"]));
    {
        let mut cfg = state.config.write().await;
        cfg.auth.oidc.enabled = false;
        cfg.auth.oidc.issuer_url.clear();
    }

    // 4b. GET /api/metrics scopes ninjanvr_camera_fps, ninjanvr_camera_online, and ninjanvr_events_total
    let clips_dir = media_dir.join("clips");
    std::fs::create_dir_all(&clips_dir).unwrap();
    let mut dummy_img = image::RgbImage::new(32, 32);
    for p in dummy_img.pixels_mut() {
        *p = image::Rgb([100, 120, 140]);
    }
    let dummy_webp = ninjanvr_vision::encode_image_webp(&dummy_img).unwrap();
    for (cam, ev_id) in [("front_door", "ev_r18_fd"), ("backyard", "ev_r18_by")] {
        std::fs::write(clips_dir.join(format!("{cam}-{ev_id}.webp")), &dummy_webp).unwrap();
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: ev_id.to_string(),
                camera: cam.to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 5000.0,
                end_time: Some(5005.0),
                has_clip: false,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.9),
                false_positive: Some(false),
                box_coords: Some(json!([0.1, 0.1, 0.5, 0.5])),
                data: json!({"box": [0.1, 0.1, 0.5, 0.5]}),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.9),
                stationary: false,
            })
            .unwrap();
        let _ =
            create_dataset_sample_from_event(&state.db, &media_dir, ev_id, "true_positive", None)
                .await
                .unwrap();
    }

    let r_metrics_scoped = auth_app
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
    assert_eq!(r_metrics_scoped.status(), StatusCode::OK);
    let metrics_scoped_text = String::from_utf8(
        to_bytes(r_metrics_scoped.into_body(), 65536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(metrics_scoped_text.contains("camera=\"front_door\""));
    assert!(
        !metrics_scoped_text.contains("camera=\"backyard\""),
        "Scoped user GET /api/metrics must never leak backyard camera metrics"
    );

    let r_metrics_admin = auth_app
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
    let metrics_admin_text = String::from_utf8(
        to_bytes(r_metrics_admin.into_body(), 65536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(metrics_admin_text.contains("camera=\"front_door\""));
    assert!(metrics_admin_text.contains("camera=\"backyard\""));

    // 4c. GET /api/dataset/summary scopes by_camera and sample totals for camera-restricted users
    let r_ds_scoped = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/dataset/summary")
                .header(header::AUTHORIZATION, &scoped_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_ds_scoped.status(), StatusCode::OK);
    let ds_scoped_json: Value =
        serde_json::from_slice(&to_bytes(r_ds_scoped.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(ds_scoped_json["total_samples"], 1);
    assert_eq!(ds_scoped_json["by_camera"]["front_door"], 1);
    assert!(
        ds_scoped_json["by_camera"].get("backyard").is_none(),
        "Scoped user GET /api/dataset/summary must never leak backyard in by_camera"
    );

    let r_ds_admin = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/dataset/summary")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let ds_admin_json: Value =
        serde_json::from_slice(&to_bytes(r_ds_admin.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(ds_admin_json["total_samples"], 2);
    assert_eq!(ds_admin_json["by_camera"]["front_door"], 1);
    assert_eq!(ds_admin_json["by_camera"]["backyard"], 1);

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn security_p2b_server_auth_rbac_share_cancel_safety_regression() {
    // 1. STRSLICE-001: sanitize_logged_path("/a€€") and GET /a%E2%82%AC%E2%82%AC do not panic
    assert_eq!(sanitize_logged_path("/a€€"), "/a€€");
    assert_eq!(
        sanitize_logged_path("/a%E2%82%AC%E2%82%AC"),
        "/a%E2%82%AC%E2%82%AC"
    );
    assert_eq!(
        sanitize_logged_path("/SHARE/tok123/video.mp4"),
        "/share/<redacted>/video.mp4"
    );

    let ctx = setup_test_app().await;
    let state = ctx.state.clone();
    let auth_app = build_router_with_auth(state.clone(), true);

    let r_utf8_path = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/a%E2%82%AC%E2%82%AC")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_utf8_path.status(), StatusCode::OK);

    // 2. STRCMP-001: case-variant OIDC/ForwardAuth user 'Alice' is capped by DB user 'alice';
    //    deleted 'admin' JWT is rejected when auth.enabled = true.
    state
        .db
        .create_user_with_cameras("alice", "pass1234", "viewer", "front_door", 1000)
        .unwrap();
    {
        let mut cfg = state.config.write().await;
        cfg.auth.proxy.enabled = true;
        cfg.auth.proxy.user_header = "Remote-User".to_string();
        cfg.auth.proxy.groups_header = "Remote-Groups".to_string();
        cfg.auth.proxy.trusted_proxies = vec!["127.0.0.1/32".to_string()];
        cfg.auth.oidc.role_mapping.admin = vec!["admins".to_string()];
    }
    let mut proxy_headers = HeaderMap::new();
    proxy_headers.insert("remote-user", "Alice".parse().unwrap());
    proxy_headers.insert("remote-groups", "admins".parse().unwrap());
    proxy_headers.insert("x-peer-ip", "127.0.0.1".parse().unwrap());
    let auth_cfg_snap = state.config.read().await.auth.clone();
    let extracted = extract_authenticated_user_with_cfg(
        &state,
        &CookieJar::new(),
        &proxy_headers,
        &auth_cfg_snap,
    );
    assert_eq!(
        extracted,
        Some(("alice".to_string(), "viewer".to_string())),
        "ForwardAuth user 'Alice' must resolve to canonical 'alice' and be capped to DB role 'viewer'"
    );
    let allowed_cams = resolve_user_allowed_cameras(
        &state,
        &CookieJar::new(),
        &proxy_headers,
        &auth_cfg_snap,
        "Alice",
        "viewer",
    );
    assert_eq!(
        allowed_cams,
        Some(vec!["front_door".to_string()]),
        "ForwardAuth user 'Alice' must be restricted to DB user 'alice' allowed_cameras"
    );
    {
        let mut cfg = state.config.write().await;
        cfg.auth.proxy.enabled = false;
    }

    let now = unix_now();
    let mint_jwt = |sub: &str, role: &str, cams: Option<&str>| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: cams.map(str::to_string),
                iat: unix_now(),
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };

    // Create second admin so we can delete 'admin' and verify stale 'admin' JWT is rejected
    state
        .db
        .create_user_with_cameras("backup_admin", "pass1234", "admin", "*", 1000)
        .unwrap();
    let stale_admin_jwt = mint_jwt("admin", "admin", Some("*"));
    assert!(state.db.delete_user("admin").unwrap());
    state.auth_gate.invalidate_user("admin");
    let auth_cfg_after_del = state.config.read().await.auth.clone();
    let mut stale_admin_hdrs = HeaderMap::new();
    stale_admin_hdrs.insert(
        header::AUTHORIZATION,
        format!("Bearer {stale_admin_jwt}").parse().unwrap(),
    );
    assert_eq!(
        extract_authenticated_user_with_cfg(
            &state,
            &CookieJar::new(),
            &stale_admin_hdrs,
            &auth_cfg_after_del,
        ),
        None,
        "Deleted 'admin' JWT must be rejected when auth.enabled = true and OIDC is inactive"
    );
    assert_eq!(
        resolve_user_allowed_cameras(
            &state,
            &CookieJar::new(),
            &stale_admin_hdrs,
            &auth_cfg_after_del,
            "admin",
            "admin",
        ),
        Some(Vec::new()),
        "Deleted 'admin' must resolve to empty allowed_cameras when auth.enabled = true"
    );
    // Restore default admin for subsequent checks
    state
        .db
        .create_user_with_cameras("admin", "adminpass123", "admin", "*", 1000)
        .unwrap();

    // 3. STRCMP-002: operator calling POST /api/exports/delete is rejected with 403 Forbidden,
    //    while operator calling DELETE /api/events/{id}/retain or DELETE /api/review/{id}/viewed is allowed
    state
        .db
        .create_user_with_cameras("op_p2b", "pass1234", "operator", "*", 1000)
        .unwrap();
    let op_bearer = format!("Bearer {}", mint_jwt("op_p2b", "operator", Some("*")));
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_p2b_1".to_string(),
            camera: "front_door".to_string(),
            name: "P2B Export".to_string(),
            date: now as f64,
            video_path: "/media/ninjanvr/exports/exp_p2b_1.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_p2b_1.webp".to_string(),
            in_progress: false,
        })
        .unwrap();

    let r_op_bulk_exp_del = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/exports/delete")
                .header(header::AUTHORIZATION, &op_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ids":["exp_p2b_1"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_op_bulk_exp_del.status(), StatusCode::FORBIDDEN);
    assert!(state.db.get_export_by_id("exp_p2b_1").unwrap().is_some());

    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_p2b_retain".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 1000.0,
            end_time: Some(1005.0),
            has_clip: false,
            has_snapshot: false,
            plus_id: None,
            retain_indefinitely: true,
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
    let r_op_retain_off = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/events/ev_p2b_retain/retain")
                .header(header::AUTHORIZATION, &op_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_op_retain_off.status(), StatusCode::OK);

    state
        .db
        .insert_review_segment(&ninjanvr_db::ReviewItemDto {
            id: "rev_p2b_viewed".to_string(),
            camera: "front_door".to_string(),
            severity: "alert".to_string(),
            start_time: 1000.0,
            end_time: Some(1010.0),
            has_been_reviewed: true,
            data: json!({}),
            thumb_path: "/media/ninjanvr/clips/front_door-rev_p2b_viewed.webp".to_string(),
        })
        .unwrap();
    let r_op_unview = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/review/rev_p2b_viewed/viewed")
                .header(header::AUTHORIZATION, &op_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_op_unview.status(), StatusCode::OK);

    // 4. CANCELSAFETY-002: dropping ShareStreamCursorGuard rolls back active_streams[idx] in share_page_video_credits
    {
        let mut map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.entry("shr_cancel_test".to_string()).or_default();
        entry.active_streams = vec![1024];
        entry.active_stream_expires = vec![now + 900];
        entry.sync_stream_summary();
    }
    let guard_payload = ShareTokenPayload {
        k: "export".to_string(),
        id: "exp_p2b_1".to_string(),
        exp: now + 3600,
        cam: Some("front_door".to_string()),
        jti: Some("shr_cancel_test".to_string()),
    };
    {
        let _guard = ShareStreamCursorGuard::new(
            state.clone(),
            guard_payload.clone(),
            Some((0, 128, now + 300)),
        );
    }
    {
        let map = state.share_engine.page_video_credits.lock().unwrap();
        let entry = map.get("shr_cancel_test").unwrap();
        assert_eq!(entry.active_streams[0], 128);
        assert_eq!(entry.active_stream_expires[0], now + 300);
    }

    // 5. FLOATEDGE-003 & ARITHOFL-001: cleanup_camera_share_clip_artifacts_in_range(NaN) is a no-op;
    //    extreme session_length clamps without overflow.
    let exports_dir = state.media_dir.join("exports");
    std::fs::create_dir_all(&exports_dir).unwrap();
    let cached_clip = exports_dir.join("share_clip_ev_p2b_retain.mp4");
    std::fs::write(&cached_clip, b"CACHED_CLIP_BYTES").unwrap();
    cleanup_camera_share_clip_artifacts_in_range(&state, "front_door", f64::NAN, now as f64);
    assert!(
        cached_clip.exists(),
        "cleanup_camera_share_clip_artifacts_in_range(NaN) must be a no-op and preserve cached artifacts"
    );
    let admin_bearer = format!("Bearer {}", mint_jwt("admin", "admin", Some("*")));
    let r_nan_del = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/front_door/recordings?after=NaN")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_nan_del.status(), StatusCode::BAD_REQUEST);
    assert!(cached_clip.exists());

    {
        let mut cfg = state.config.write().await;
        cfg.auth.session_length = u64::MAX;
    }
    let r_login_extreme = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"user":"admin","password":"adminpass123"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_login_extreme.status(), StatusCode::OK);
}

#[tokio::test]
async fn security_p3a_server_io_dos_mutations_regression() {
    // 1. RESEXHAUST-002 & FLOATEDGE-004: extract_xml_element_text on 40,000 unclosed <Fault> tags
    //    completes in < 250ms (O(N)); ONVIF SOAP builders clamp NaN to finite defaults.
    let unclosed_faults = "<Fault>".repeat(40_000);
    let t0 = Instant::now();
    assert_eq!(extract_xml_element_text(&unclosed_faults, "Fault"), None);
    let elapsed_xml = t0.elapsed();
    assert!(
        elapsed_xml < Duration::from_millis(250),
        "extract_xml_element_text on 40,000 unclosed <Fault> tags must run in < 250ms (took {elapsed_xml:?})"
    );

    let cont_soap = build_onvif_continuous_move_soap(
        "MainProfile",
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        10,
        None,
        None,
    );
    assert!(!cont_soap.contains("NaN") && !cont_soap.contains("inf"));
    assert!(cont_soap.contains("x=\"0.0000\" y=\"0.0000\""));
    assert!(cont_soap.contains("<tt:Zoom x=\"0.0000\"/>"));

    let rel_soap = build_onvif_relative_move_soap(
        "MainProfile",
        f32::NAN,
        f32::NAN,
        f32::NAN,
        Some(f32::NAN),
        None,
        None,
    );
    assert!(!rel_soap.contains("NaN") && !rel_soap.contains("inf"));
    assert!(rel_soap.contains("x=\"0.0000\" y=\"0.0000\""));
    assert!(rel_soap.contains("x=\"0.5000\" y=\"0.5000\""));

    let preset_soap =
        build_onvif_goto_preset_soap("MainProfile", "Preset1", Some(f32::NAN), None, None);
    assert!(!preset_soap.contains("NaN") && !preset_soap.contains("inf"));
    assert!(preset_soap.contains("x=\"0.5000\" y=\"0.5000\""));

    let ptz_nan = PtzMoveBody {
        action: Some("left".to_string()),
        direction: None,
        mode: None,
        pan: Some(f32::NAN),
        tilt: Some(f32::NAN),
        zoom: Some(f32::NAN),
        speed: Some(f32::NAN),
        timeout_sec: None,
        timeout_ms: None,
        profile_token: None,
    };
    let (v_pan, v_tilt, v_zoom) = ptz_nan.resolve_vectors();
    assert!(v_pan.is_finite() && v_tilt.is_finite() && v_zoom.is_finite());
    assert!((v_pan - (-0.5)).abs() < 1e-5);
    assert_eq!(v_tilt, 0.0);
    assert_eq!(v_zoom, 0.0);

    // 2. RESEXHAUST-005: split_csv_opt caps > 64 tokens at 64.
    let many_csv = Some(
        (0..200)
            .map(|i| format!("tok_{i}"))
            .collect::<Vec<_>>()
            .join(","),
    );
    let parsed_csv = split_csv_opt(&many_csv).unwrap();
    assert_eq!(parsed_csv.len(), 64);
    assert_eq!(parsed_csv[0], "tok_0");
    assert_eq!(parsed_csv[63], "tok_63");

    // 3. ARITHOFL-003: build_zip_archive boundary check and non-duplicating incremental builder.
    let mut zb = ZipArchiveBuilder::new();
    assert!(zb.push_entry("data.yaml", b"nc: 15\n"));
    assert!(zb.push_entry("images/train/s1.webp", b"WEBP_DATA"));
    // Entry whose projected archive size would exceed u32::MAX is rejected by can_fit_entry
    assert!(!zb.can_fit_entry("images/train/huge.webp", u32::MAX as usize - 10));
    let zip_out = zb.finish();
    let parsed_zip = parse_zip_archive(&zip_out).unwrap();
    assert_eq!(parsed_zip.get("data.yaml").unwrap(), b"nc: 15\n");
    assert_eq!(
        parsed_zip.get("images/train/s1.webp").unwrap(),
        b"WEBP_DATA"
    );

    // 4. RESEXHAUST-006 & RESEXHAUST-007: viewer calling POST /api/debug/reprocess is rejected with 403 Forbidden;
    //    existing_recordings_for_vod and VOD endpoints reject (end - start) > 86400.0.
    let ctx = setup_test_app().await;
    let state = ctx.state.clone();
    let auth_app = build_router_with_auth(state.clone(), true);
    let now = unix_now();
    let mint_jwt = |sub: &str, role: &str, cams: Option<&str>| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: cams.map(str::to_string),
                iat: unix_now(),
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };
    state
        .db
        .create_user_with_cameras("viewer_p3a", "pass1234", "viewer", "*", 1000)
        .unwrap();
    let viewer_bearer = format!("Bearer {}", mint_jwt("viewer_p3a", "viewer", Some("*")));
    let admin_bearer = format!("Bearer {}", mint_jwt("admin", "admin", Some("*")));

    let r_viewer_reproc = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/debug/reprocess")
                .header(header::AUTHORIZATION, &viewer_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"camera":"front_door","start_time":1000.0,"end_time":1010.0}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_viewer_reproc.status(),
        StatusCode::FORBIDDEN,
        "viewer calling POST /api/debug/reprocess must be rejected with 403 Forbidden"
    );

    assert!(
        existing_recordings_for_vod(&state, "front_door", 0.0, 86_401.0).is_empty(),
        "existing_recordings_for_vod must reject (end - start) > 86400.0"
    );
    for vod_uri in [
        "/vod/front_door/start/0/end/86401/master.m3u8",
        "/vod/front_door/start/0/end/86401/index.m3u8",
        "/vod/front_door/start/0/end/86401/init-v1-a1.mp4",
        "/vod/front_door/start/0/end/86401/seg-1-v1-a1.m4s",
        "/api/front_door/recordings?after=NaN",
        "/api/front_door/recordings?after=500&before=100",
    ] {
        let r_vod = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(vod_uri)
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_vod.status(),
            StatusCode::BAD_REQUEST,
            "Expected 400 BAD_REQUEST on {vod_uri}"
        );
    }
}

#[tokio::test]
async fn security_p3_server_audit_remediation_regression() {
    let ctx = setup_test_app().await;
    let state = ctx.state.clone();
    let auth_app = build_router_with_auth(state.clone(), true);
    let now = unix_now();
    let mint_jwt = |sub: &str, role: &str, cams: Option<&str>| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: cams.map(str::to_string),
                iat: unix_now(),
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };
    state
        .db
        .create_user_with_cameras("op_audit", "pass1234", "operator", "*", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("viewer_audit", "pass1234", "viewer", "*", 1000)
        .unwrap();
    let admin_bearer = format!("Bearer {}", mint_jwt("admin", "admin", Some("*")));
    let op_bearer = format!("Bearer {}", mint_jwt("op_audit", "operator", Some("*")));
    let viewer_bearer = format!("Bearer {}", mint_jwt("viewer_audit", "viewer", Some("*")));

    // =========================================================================
    // 1. LOSSYFROM-001: Export Timestamp Upper-Bound Rejection, Sub-Second Non-Collision,
    //    and Protection of Admin-Owned Exports Against Operator Overwrite/Deletion
    // =========================================================================
    for bad_export_uri in [
        "/api/export/front_door/start/4102444801/end/4102444810",
        "/api/export/front_door/start/4102444795/end/4102444805",
        "/api/export/front_door/start/18446744073709552000/end/18446744073709556096",
    ] {
        let r_oob = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(bad_export_uri)
                    .header(header::AUTHORIZATION, &op_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_oob.status(),
            StatusCode::BAD_REQUEST,
            "Out-of-bounds export timestamp {bad_export_uri} must return 400 BAD_REQUEST"
        );
    }

    // Seed a protected existing export `front_door_7000_7010` on disk and in SQLite
    let exports_dir = state.media_dir.join("exports");
    let clips_dir = state.media_dir.join("clips");
    std::fs::create_dir_all(&exports_dir).unwrap();
    std::fs::create_dir_all(&clips_dir).unwrap();
    let protected_id = "front_door_7000_7010";
    let protected_mp4 = exports_dir.join(format!("{protected_id}.mp4"));
    let protected_webp = clips_dir.join(format!("{protected_id}.webp"));
    std::fs::write(&protected_mp4, b"ADMIN_PROTECTED_MP4").unwrap();
    std::fs::write(&protected_webp, b"ADMIN_PROTECTED_WEBP").unwrap();
    state
        .db
        .insert_export(&ExportRow {
            id: protected_id.to_string(),
            camera: "front_door".to_string(),
            name: "Admin Evidence Clip".to_string(),
            date: now as f64,
            video_path: format!("/media/ninjanvr/exports/{protected_id}.mp4"),
            thumb_path: format!("/media/ninjanvr/clips/{protected_id}.webp"),
            in_progress: false,
        })
        .unwrap();

    // Operator attempts both exact integer `/start/7000/end/7010` and sub-second `/start/7000.1/end/7010.9`
    // (and same-second `/start/7000.1/end/7000.9`) when no source recordings exist for 7000..7010
    // (so build_mp4_export_with_privacy fails and triggers ExportInProgressGuard::drop).
    for op_export_uri in [
        "/api/export/front_door/start/7000/end/7010",
        "/api/export/front_door/start/7000.1/end/7010.9",
        "/api/export/front_door/start/7000.1/end/7000.9",
    ] {
        let r_op_exp = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(op_export_uri)
                    .header(header::AUTHORIZATION, &op_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"name":"op_overwrite_attempt"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_op_exp.status(), StatusCode::BAD_REQUEST);
        let surviving = state
            .db
            .get_export_by_id(protected_id)
            .unwrap()
            .expect("Admin export row must survive operator colliding export request");
        assert_eq!(surviving["name"], "Admin Evidence Clip");
        assert_eq!(surviving["in_progress"], false);
        assert_eq!(
            std::fs::read(&protected_mp4).unwrap(),
            b"ADMIN_PROTECTED_MP4",
            "Admin export MP4 file must never be unlinked or overwritten by operator"
        );
        assert_eq!(
            std::fs::read(&protected_webp).unwrap(),
            b"ADMIN_PROTECTED_WEBP",
            "Admin export WebP thumbnail must never be unlinked or overwritten by operator"
        );
    }

    // =========================================================================
    // 2. STRCMP-001 & STRCMP-003: Segment Boundaries in auth_gate_middleware &
    //    Case-Insensitive Host Comparison in is_internal_backend_host / federation
    // =========================================================================
    {
        let mut cfg = state.config.write().await;
        let base_cam = cfg.cameras.get("front_door").unwrap().clone();
        cfg.cameras.insert("mcp_cam".to_string(), base_cam.clone());
        cfg.cameras
            .insert("logout_cam".to_string(), base_cam.clone());
        cfg.cameras.insert("users_lobby".to_string(), base_cam);
    }
    // Viewer MUST be blocked from mutating `/api/mcp_cam/state` and `/api/logout_cam/state`
    for mut_cam_uri in ["/api/mcp_cam/state", "/api/logout_cam/state"] {
        let r_viewer_mut = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(mut_cam_uri)
                    .header(header::AUTHORIZATION, &viewer_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"detect":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_viewer_mut.status(),
            StatusCode::FORBIDDEN,
            "Viewer must be blocked from mutating prefix-colliding camera route {mut_cam_uri}"
        );
    }
    // Viewer and Operator MUST be allowed to read `/api/users_lobby/latest.jpg` (not falsely blocked as `/api/users`)
    let r_viewer_users_cam = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/users_lobby/latest.jpg")
                .header(header::AUTHORIZATION, &viewer_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_viewer_users_cam.status(),
        StatusCode::OK,
        "Camera named users_lobby must not be falsely rejected by /api/users admin-only prefix"
    );

    assert!(is_internal_backend_host(
        "NINJANVR.DEFAULT.SVC.CLUSTER.LOCAL.:5000"
    ));
    assert!(is_internal_backend_host("gateway.INTERNAL.:8443"));
    assert!(!is_safe_federation_peer_base_url(
        "http://sub.METADATA.GOOGLE.INTERNAL./api"
    ));

    // =========================================================================
    // 3. RESEXHAUST-002 & ASYNCBLOCK-005: Bounded Snapshot Decode & Masked Snapshot Cache
    // =========================================================================
    // Craft a PNG header claiming 16384x16384 dimensions (> 8192 limit)
    let mut bomb_png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bomb_png.extend_from_slice(&13u32.to_be_bytes());
    bomb_png.extend_from_slice(b"IHDR");
    bomb_png.extend_from_slice(&16384u32.to_be_bytes());
    bomb_png.extend_from_slice(&16384u32.to_be_bytes());
    bomb_png.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
    assert!(
        decode_bounded_snapshot_rgb(&bomb_png).is_none(),
        "decode_bounded_snapshot_rgb must reject 16384x16384 image header before allocating"
    );
    let (fallback_redacted, mime) = redact_image_bytes_with_privacy_mask(
        &bomb_png,
        Some(&json!(["0.1,0.1,0.5,0.1,0.5,0.5,0.1,0.5"])),
        false,
    );
    assert_eq!(mime, "image/webp");
    assert!(!fallback_redacted.is_empty());

    // =========================================================================
    // 4. CANCELSAFETY-002 & TOCTOU-011: Atomic Config Persistence & Symlink Rejection
    // =========================================================================
    let r_cfg_struct = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config/structured")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"mqtt":{"enabled":false}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_cfg_struct.status(), StatusCode::OK);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let cfg_mode = std::fs::metadata(&ctx.cfg_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            cfg_mode, 0o600,
            "config.yml persisted by api_config_structured must enforce 0o600 permissions"
        );
        let outside_target = state.media_dir.join("outside_secret_cfg.yml");
        std::fs::write(&outside_target, b"DO_NOT_OVERWRITE_SECRET").unwrap();
        let sym_cfg = state.media_dir.join("symlink_cfg.yml");
        std::os::unix::fs::symlink(&outside_target, &sym_cfg).unwrap();
        assert!(
            persist_config_yaml_atomically(&sym_cfg, "cameras: {}\n").is_err(),
            "persist_config_yaml_atomically must refuse to overwrite a symlink"
        );
        assert_eq!(
            std::fs::read(&outside_target).unwrap(),
            b"DO_NOT_OVERWRITE_SECRET"
        );

        // =========================================================================
        // 5. CYCLE-2: Intra-media_dir Symlink Rejection in ensure_path_within_media_dir
        // =========================================================================
        let sym_in_clips = clips_dir.join("front_door-sym_test-clean.webp");
        std::os::unix::fs::symlink(&protected_webp, &sym_in_clips).unwrap();
        assert!(
            ensure_path_within_media_dir(&state.media_dir, &sym_in_clips).is_none(),
            "ensure_path_within_media_dir must reject symlinks even when target is inside media_dir"
        );
        assert!(
            ensure_clips_artifact_path(&state.media_dir, &sym_in_clips).is_none(),
            "ensure_clips_artifact_path must reject intra-media_dir symlinks"
        );
        let _ = std::fs::remove_file(&sym_in_clips);
    }

    // =========================================================================
    // 6. CYCLE-2: Privacy-Mask Normalization & Fail-Closed Redaction Across Formats + MCP
    // =========================================================================
    let bright_img = image::RgbImage::from_pixel(64, 36, image::Rgb([240, 240, 240]));
    let bright_webp = ninjanvr_vision::encode_image_webp(&bright_img).unwrap();
    for valid_mask in [
        json!({"zones": ["0.0,0.0,1.0,0.0,1.0,1.0,0.0,1.0"]}),
        json!("0.0,0.0,1.0,1.0"),
        json!([0.0, 0.0, 1.0, 1.0]),
        json!([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]),
        json!(["corrupt_non_empty_mask"]),
        json!(true),
    ] {
        let (redacted_out, _) =
            redact_image_bytes_with_privacy_mask(&bright_webp, Some(&valid_mask), false);
        let decoded_out = image::load_from_memory(&redacted_out).unwrap().to_rgb8();
        let center_px = decoded_out.get_pixel(32, 18);
        assert!(
            center_px[0] < 50 && center_px[1] < 50 && center_px[2] < 50,
            "redact_image_bytes_with_privacy_mask must redact or fail-closed for mask {valid_mask}, got pixel {center_px:?}"
        );
    }

    // MCP search_events must also redact thumbnails when camera privacy_mask is active
    {
        let mut cfg = state.config.write().await;
        cfg.cameras.get_mut("front_door").unwrap().privacy_mask =
            json!({"zones": ["0.0,0.0,1.0,0.0,1.0,1.0,0.0,1.0"]});
    }
    let unredacted_b64 = base64::engine::general_purpose::STANDARD.encode(&bright_webp);
    state
        .db
        .upsert_event(&ninjanvr_db::EventDto {
            id: "ev_mcp_mask_test".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: now as f64,
            end_time: Some((now + 5) as f64),
            has_clip: false,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.92),
            false_positive: Some(false),
            box_coords: Some(json!([0.1, 0.1, 0.5, 0.5])),
            data: json!({"type": "object", "score": 0.92}),
            thumbnail: Some(unredacted_b64.clone()),
            area: Some(1000),
            ratio: Some(1.0),
            score: Some(0.92),
            stationary: false,
        })
        .unwrap();
    let r_mcp_search = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mcp")
                    .header(header::AUTHORIZATION, &viewer_bearer)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_events","arguments":{"camera":"front_door","limit":5}}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(r_mcp_search.status(), StatusCode::OK);
    let mcp_body_bytes = axum::body::to_bytes(r_mcp_search.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let mcp_json: Value = serde_json::from_slice(&mcp_body_bytes).unwrap();
    let mcp_ev = mcp_json["result"]["structuredContent"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "ev_mcp_mask_test")
        .expect("expected ev_mcp_mask_test in MCP search_events response");
    let mcp_thumb_b64 = mcp_ev["thumbnail"].as_str().unwrap();
    assert_ne!(
        mcp_thumb_b64, unredacted_b64,
        "MCP search_events must redact base64 event thumbnails for masked cameras"
    );
}

#[tokio::test]
async fn phase1_onvif_wsse_password_digest_profile_discovery_and_preset_crud_work() {
    // 1. RFC 3174 SHA-1 test vectors and deterministic WSSE PasswordDigest verification
    assert_eq!(
        sha1_digest(b"abc")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    assert_eq!(
        sha1_digest(b"")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        "da39a3ee5e6b4b0d3255bfef95601890afd80709"
    );
    let fixed_nonce = [1u8; 16];
    let digest_hdr =
        build_onvif_wsse_digest_header("admin", "secret_pw", &fixed_nonce, "2026-11-07T02:00:00Z");
    assert!(digest_hdr.contains("#PasswordDigest\">"));
    assert!(digest_hdr.contains("<wsse:Nonce"));
    assert!(digest_hdr.contains("<wsu:Created>2026-11-07T02:00:00Z</wsu:Created>"));
    assert!(!digest_hdr.contains("secret_pw"));

    // 2. Mock ONVIF server that strictly requires:
    //    - WS-UsernameToken PasswordDigest (rejects plaintext wsse:Password with NotAuthorized)
    //    - Profile000 (rejects Profile_1 with ter:NoProfile)
    //    - Supports GetStatus, ContinuousMove, RelativeMove, AbsoluteMove, and tests both
    //      native ONVIF SetPreset/RemovePreset AND ActionNotSupported soft-preset fallback.
    let preset_mode = Arc::new(std::sync::Mutex::new("native".to_string()));
    let preset_mode_srv = preset_mode.clone();
    let strict_onvif_router = Router::new().route(
            "/onvif/ptz_service",
            post(move |body: String| {
                let pmode = preset_mode_srv.lock().unwrap().clone();
                async move {
                    if !body.contains("#PasswordDigest")
                        || !body.contains("<wsse:Nonce")
                        || !body.contains("<wsu:Created>")
                    {
                        return (
                            StatusCode::BAD_REQUEST,
                            [(header::CONTENT_TYPE, "application/soap+xml")],
                            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><s:Fault><s:Reason><s:Text>Sender not Authorized</s:Text></s:Fault></s:Body></s:Envelope>"#.to_string(),
                        );
                    }
                    if !body.contains("<tptz:ProfileToken>Profile000</tptz:ProfileToken>") {
                        return (
                            StatusCode::BAD_REQUEST,
                            [(header::CONTENT_TYPE, "application/soap+xml")],
                            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><s:Fault><s:Code><s:Subcode><s:Value>ter:NoProfile</s:Value></s:Subcode></s:Code><s:Reason><s:Text>Profile token does not exist</s:Text></s:Fault></s:Body></s:Envelope>"#.to_string(),
                        );
                    }
                    if (body.contains("SetPreset")
                        || body.contains("GotoPreset")
                        || body.contains("GetPresets"))
                        && pmode == "unsupported"
                    {
                        return (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/soap+xml")],
                            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><s:Fault><s:Code><s:Subcode><s:Value>ter:ActionNotSupported</s:Value></s:Subcode></s:Code><s:Reason><s:Text>This optional method is not implemented</s:Text></s:Fault></s:Body></s:Envelope>"#.to_string(),
                        );
                    }
                    let resp_xml = if body.contains("GetStatus") {
                        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body><tptz:GetStatusResponse><tptz:PTZStatus><tt:Position><tt:PanTilt x="0.4000" y="-0.2000"/></tt:Position><tt:MoveStatus><tt:PanTilt>IDLE</tt:PanTilt></tt:MoveStatus></tptz:PTZStatus></tptz:GetStatusResponse></s:Body></s:Envelope>"#
                    } else if body.contains("GetPresets") {
                        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body><tptz:GetPresetsResponse><tptz:Preset token="1"><tt:Name>Home</tt:Name></tptz:Preset></tptz:GetPresetsResponse></s:Body></s:Envelope>"#
                    } else if body.contains("SetPreset") {
                        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><s:Body><tptz:SetPresetResponse><tptz:PresetToken>2</tptz:PresetToken></tptz:SetPresetResponse></s:Body></s:Envelope>"#
                    } else if body.contains("RemovePreset") {
                        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><s:Body><tptz:RemovePresetResponse/></s:Body></s:Envelope>"#
                    } else if body.contains("AbsoluteMove") {
                        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tptz:AbsoluteMoveResponse/></s:Body></s:Envelope>"#
                    } else {
                        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tptz:RelativeMoveResponse/></s:Body></s:Envelope>"#
                    };
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/soap+xml")],
                        resp_xml.to_string(),
                    )
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, strict_onvif_router).await;
    });

    let ctx = setup_test_app().await;
    let app = &ctx.app;
    let state = &ctx.state;
    {
        let mut cfg_guard = state.config.write().await;
        if let Some(cam) = cfg_guard.cameras.get_mut("front_door") {
            cam.onvif = ninjanvr_config::OnvifCameraConfig {
                host: Some("127.0.0.1".to_string()),
                port: addr.port(),
                user: Some("admin".to_string()),
                password: Some("digest_secret".to_string()),
                autotracking: false,
                return_preset: None,
                timeout_sec: 2,
            };
        }
    }

    // A. GET /api/front_door/ptz/info auto-discovers Profile000 and authenticates with PasswordDigest
    let r_info = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/ptz/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_info.status(), StatusCode::OK);
    let info_j: Value =
        serde_json::from_slice(&to_bytes(r_info.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(info_j["online"], true);
    assert_eq!(info_j["profile_token"], "Profile000");

    // B. POST /api/front_door/ptz/move with mode="absolute" uses discovered Profile000
    let r_abs = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/ptz/move")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"mode":"absolute","pan":0.25,"tilt":-0.1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_abs.status(), StatusCode::OK);
    let abs_j: Value =
        serde_json::from_slice(&to_bytes(r_abs.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(abs_j["operation"], "AbsoluteMove");

    // C. Native ONVIF SetPreset & RemovePreset
    let r_save = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/ptz/preset/save")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"KitchenView"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_save.status(), StatusCode::OK);
    let save_j: Value =
        serde_json::from_slice(&to_bytes(r_save.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(save_j["token"], "2");
    assert_eq!(save_j["source"], "onvif");

    let r_del = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/front_door/ptz/preset/2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_del.status(), StatusCode::OK);

    // D. Soft-Preset fallback when camera returns ter:ActionNotSupported (Intelbras iME-360-C)
    *preset_mode.lock().unwrap() = "unsupported".to_string();
    let r_soft_save = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/ptz/preset/save")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"SalaPorta"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_soft_save.status(), StatusCode::OK);
    let soft_save_j: Value =
        serde_json::from_slice(&to_bytes(r_soft_save.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(soft_save_j["source"], "soft_preset");

    let r_soft_goto = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/ptz/preset")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"preset":"SalaPorta"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_soft_goto.status(), StatusCode::OK);
    let soft_goto_j: Value =
        serde_json::from_slice(&to_bytes(r_soft_goto.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(soft_goto_j["source"], "soft_preset");

    let r_soft_del = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/front_door/ptz/preset/SalaPorta")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_soft_del.status(), StatusCode::OK);
    let soft_del_j: Value =
        serde_json::from_slice(&to_bytes(r_soft_del.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(soft_del_j["source"], "soft_preset");

    srv_handle.abort();
}

#[tokio::test]
async fn phase2_phase3_camera_hardware_capabilities_dahua_cgi_and_talkback_work() {
    // 1. RFC 1321 MD5 test vectors
    assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(
        md5_hex(b"message digest"),
        "f96b697d7cb7938d525a2f31aaf161d0"
    );

    // 2. HTTP Digest Authentication header generation (with qop="auth" and empty opaque="")
    let www_auth =
        r#"Digest realm="Login to 5C93B8D566124B9A", qop="auth", nonce="5c6338dc", opaque="""#;
    let digest_hdr = build_http_digest_auth_header(
        www_auth,
        "GET",
        "/cgi-bin/magicBox.cgi?action=getSystemInfo",
        "admin",
        "cam_pass",
        "0a4f113b",
    )
    .unwrap();
    assert!(digest_hdr.contains("username=\"admin\""));
    assert!(digest_hdr.contains("realm=\"Login to 5C93B8D566124B9A\""));
    assert!(digest_hdr.contains("opaque=\"\""));
    assert!(digest_hdr.contains("qop=auth, nc=00000001, cnonce=\"0a4f113b\""));

    let rt_nc = CameraDriverRuntime::new();
    rt_nc.set_digest_challenge("http://192.0.2.70:80", www_auth);
    let (_, nc2) = rt_nc.next_digest_challenge("http://192.0.2.70:80").unwrap();
    let (_, nc3) = rt_nc.next_digest_challenge("http://192.0.2.70:80").unwrap();
    assert_eq!(nc2, 2);
    assert_eq!(nc3, 3);
    let digest_hdr_nc2 = camera_protocols::build_http_digest_auth_header_with_nc(
        www_auth,
        "GET",
        "/cgi-bin/configManager.cgi?action=getConfig&name=Lighting",
        "admin",
        "cam_pass",
        "0a4f113b",
        nc2,
    )
    .unwrap();
    assert!(digest_hdr_nc2.contains("qop=auth, nc=00000002, cnonce=\"0a4f113b\""));
    assert_ne!(digest_hdr, digest_hdr_nc2);

    // 3. ITU-T G.711 A-law encoding and WAV stripping (including odd-length RIFF chunk padding)
    assert_eq!(linear_pcm16_to_alaw(0), 0xD5);
    let mut fake_wav = Vec::new();
    fake_wav.extend_from_slice(b"RIFF");
    fake_wav.extend_from_slice(&52u32.to_le_bytes());
    fake_wav.extend_from_slice(b"WAVEfmt ");
    fake_wav.extend_from_slice(&16u32.to_le_bytes());
    fake_wav.extend_from_slice(&1u16.to_le_bytes()); // PCM format = 1
    fake_wav.extend_from_slice(&1u16.to_le_bytes()); // channels = 1
    fake_wav.extend_from_slice(&8000u32.to_le_bytes()); // sample_rate = 8000
    fake_wav.extend_from_slice(&16000u32.to_le_bytes()); // byte_rate = 16000
    fake_wav.extend_from_slice(&2u16.to_le_bytes()); // block_align = 2
    fake_wav.extend_from_slice(&16u16.to_le_bytes()); // bits_per_sample = 16
    // Odd-sized JUNK chunk (3 bytes + 1 RIFF pad byte)
    fake_wav.extend_from_slice(b"JUNK");
    fake_wav.extend_from_slice(&3u32.to_le_bytes());
    fake_wav.extend_from_slice(&[1, 2, 3, 0]);
    fake_wav.extend_from_slice(b"data");
    fake_wav.extend_from_slice(&4u32.to_le_bytes());
    fake_wav.extend_from_slice(&0i16.to_le_bytes());
    fake_wav.extend_from_slice(&1024i16.to_le_bytes());
    let alaw_out = convert_pcm16le_or_wav_to_alaw(&fake_wav);
    assert_eq!(alaw_out.len(), 2);
    assert_eq!(alaw_out[0], 0xD5);

    // 4. Mock Intelbras iME-360-C (ONVIF + Dahua HTTP Digest CGI + audio.cgi talkback)
    let received_talkback_bytes = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let received_set_configs = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let tb_srv = received_talkback_bytes.clone();
    let sc_srv = received_set_configs.clone();

    let mock_cam_router = Router::new()
            .route(
                "/onvif/device_service",
                post(|| async {
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/soap+xml")],
                        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:GetDeviceInformationResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:Manufacturer>IntelBras</tds:Manufacturer><tds:Model>iME-360-C</tds:Model><tds:FirmwareVersion>2.800.00IB007.0.R</tds:FirmwareVersion><tds:SerialNumber>SN12345</tds:SerialNumber><tds:HardwareId>1.00</tds:HardwareId></tds:GetDeviceInformationResponse></s:Body></s:Envelope>"#,
                    )
                }),
            )
            .route(
                "/onvif/ptz_service",
                post(|| async {
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/soap+xml")],
                        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tptz:GetStatusResponse xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><tptz:PTZStatus><tt:Position><tt:PanTilt x="0.8000" y="0.8000"/></tt:Position><tt:MoveStatus><tt:Zoom>IDLE</tt:Zoom></tt:MoveStatus></tptz:PTZStatus></tptz:GetStatusResponse></s:Body></s:Envelope>"#,
                    )
                }),
            )
            .route(
                "/onvif/imaging_service",
                post(|body: String| async move {
                    if body.contains("SetImagingSettings") {
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/soap+xml")],
                            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><timg:SetImagingSettingsResponse xmlns:timg="http://www.onvif.org/ver20/imaging/wsdl"/></s:Body></s:Envelope>"#,
                        )
                    } else {
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/soap+xml")],
                            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><timg:GetImagingSettingsResponse xmlns:timg="http://www.onvif.org/ver20/imaging/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><timg:ImagingSettings><tt:Brightness>55.0</tt:Brightness><tt:ColorSaturation>60.0</tt:ColorSaturation><tt:Contrast>50.0</tt:Contrast><tt:Sharpness>45.0</tt:Sharpness></timg:ImagingSettings></timg:GetImagingSettingsResponse></s:Body></s:Envelope>"#,
                        )
                    }
                }),
            )
            .route(
                "/cgi-bin/magicBox.cgi",
                get(|headers: HeaderMap, uri: Uri| async move {
                    let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            [(
                                header::WWW_AUTHENTICATE,
                                r#"Digest realm="IntelbrasIPC", qop="auth", nonce="n1234", opaque="""#,
                            )],
                            String::new(),
                        )
                            .into_response();
                    };
                    if !auth.starts_with("Digest ") || !auth.contains("opaque=\"\"") {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    let q = uri.query().unwrap_or("");
                    if q.contains("action=reboot") {
                        return (StatusCode::OK, "OK\r\n").into_response();
                    }
                    (
                        StatusCode::OK,
                        "serialNumber=SN12345\r\ndeviceType=iME-360-C\r\nprocessor=FH8626\r\n",
                    )
                        .into_response()
                }),
            )
            .route(
                "/cgi-bin/configManager.cgi",
                get(move |headers: HeaderMap, uri: Uri| {
                    let sc_log = sc_srv.clone();
                    async move {
                        let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
                            return (
                                StatusCode::UNAUTHORIZED,
                                [(
                                    header::WWW_AUTHENTICATE,
                                    r#"Digest realm="IntelbrasIPC", qop="auth", nonce="n1234", opaque="""#,
                                )],
                                String::new(),
                            )
                                .into_response();
                        };
                        if !auth.starts_with("Digest ") {
                            return StatusCode::UNAUTHORIZED.into_response();
                        }
                        let q = uri.query().unwrap_or("");
                        if q.starts_with("action=setConfig&") {
                            sc_log.lock().unwrap().push(q.to_string());
                            return (StatusCode::OK, "OK\r\n").into_response();
                        }
                        let body = if q.contains("name=LeSmartTrack") {
                            "table.LeSmartTrack[0].Enable=true\r\n"
                        } else if q.contains("name=VideoInOptions") {
                            "table.VideoInOptions[0].DayNightColor=1\r\ntable.VideoInOptions[0].Flip=false\r\ntable.VideoInOptions[0].Mirror=true\r\ntable.VideoInOptions[0].WideDynamicRangeMode=1\r\n"
                        } else if q.contains("name=Lighting") {
                            "table.Lighting[0][0].Mode=Auto\r\ntable.Lighting[0][0].MiddleLight[0].Light=65\r\n"
                        } else if q.contains("name=LightGlobal") {
                            "table.LightGlobal[0].Enable=true\r\n"
                        } else if q.contains("name=MotionDetect") {
                            "table.MotionDetect[0].EventHandler.BeepEnable=false\r\n"
                        } else if q.contains("name=AudioDetect") {
                            "table.AudioDetect[0].Enable=true\r\ntable.AudioDetect[0].AnomalySensitive=70\r\ntable.AudioDetect[0].EventHandler.BeepEnable=true\r\n"
                        } else if q.contains("name=AudioOutputVolume") {
                            "table.AudioOutputVolume[0]=85\r\n"
                        } else if q.contains("name=AudioInputVolume") {
                            "table.AudioInputVolume[0]=90\r\n"
                        } else {
                            ""
                        };
                        (StatusCode::OK, body).into_response()
                    }
                }),
            )
            .route(
                "/cgi-bin/audio.cgi",
                post(move |headers: HeaderMap, body: axum::body::Bytes| {
                    let tb_log = tb_srv.clone();
                    async move {
                        let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
                            return (
                                StatusCode::UNAUTHORIZED,
                                [(
                                    header::WWW_AUTHENTICATE,
                                    r#"Digest realm="IntelbrasIPC", qop="auth", nonce="n1234", opaque="""#,
                                )],
                                String::new(),
                            )
                                .into_response();
                        };
                        if !auth.starts_with("Digest ") {
                            return StatusCode::UNAUTHORIZED.into_response();
                        }
                        tb_log.lock().unwrap().extend_from_slice(&body);
                        (StatusCode::OK, "OK").into_response()
                    }
                }),
            );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, mock_cam_router).await;
    });

    let ctx = setup_test_app().await;
    let app = &ctx.app;
    let state = &ctx.state;
    {
        let mut cfg_guard = state.config.write().await;
        if let Some(cam) = cfg_guard.cameras.get_mut("front_door") {
            cam.onvif = ninjanvr_config::OnvifCameraConfig {
                host: Some("127.0.0.1".to_string()),
                port: addr.port(),
                user: Some("admin".to_string()),
                password: Some("mibo_pass".to_string()),
                autotracking: false,
                return_preset: None,
                timeout_sec: 2,
            };
        }
    }

    // A. GET /api/front_door/hardware discovers Dahua/Intelbras CGI + ONVIF capabilities & state
    let r_hw = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/hardware")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_hw.status(), StatusCode::OK);
    let hw_j: Value =
        serde_json::from_slice(&to_bytes(r_hw.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(hw_j["online"], true);
    assert_eq!(hw_j["protocol_family"], "dahua_cgi");
    assert_eq!(hw_j["device_info"]["manufacturer"], "IntelBras");
    assert_eq!(hw_j["device_info"]["model"], "iME-360-C");
    assert_eq!(hw_j["capabilities"]["ptz"], true);
    assert_eq!(hw_j["capabilities"]["ptz_zoom"], false);
    assert_eq!(hw_j["capabilities"]["push_to_talk"], true);
    assert_eq!(hw_j["capabilities"]["push_to_talk_mode"], "cgi_alaw");
    assert_eq!(hw_j["capabilities"]["imaging"], true);
    assert_eq!(hw_j["capabilities"]["hardware_autotracking"], true);
    assert_eq!(hw_j["state"]["brightness"], 55);
    assert_eq!(hw_j["state"]["hardware_autotracking"], true);
    assert_eq!(hw_j["state"]["day_night_mode"], "day");
    assert_eq!(hw_j["state"]["mirror"], true);
    assert_eq!(hw_j["state"]["speaker_volume"], 85);

    // B. POST /api/front_door/hardware updates imaging + CGI settings
    let r_set = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/front_door/hardware")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"brightness":65,"hardware_autotracking":false,"day_night_mode":"night","status_led":false,"speaker_volume":75}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(r_set.status(), StatusCode::OK);
    let set_j: Value =
        serde_json::from_slice(&to_bytes(r_set.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(set_j["success"], true);
    let logged_sc = received_set_configs.lock().unwrap().clone();
    assert_eq!(logged_sc.len(), 1);
    assert!(logged_sc[0].contains("LeSmartTrack[0].Enable=false"));
    assert!(logged_sc[0].contains("VideoInOptions[0].DayNightColor=2"));
    assert!(logged_sc[0].contains("LightGlobal[0].Enable=false"));
    assert!(logged_sc[0].contains("AudioOutputVolume[0]=75"));

    // C. POST /api/front_door/audio/talkback converts 16-bit PCM to G.711A and streams via Digest Auth
    let pcm_samples = vec![0u8; 640]; // 320 16-bit silence samples -> 320 A-law 0xD5 bytes
    let r_tb = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/audio/talkback")
                .header(header::CONTENT_TYPE, "audio/pcm")
                .body(Body::from(pcm_samples))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_tb.status(), StatusCode::OK);
    let tb_j: Value =
        serde_json::from_slice(&to_bytes(r_tb.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(tb_j["success"], true);
    assert_eq!(tb_j["bytes_sent"], 320);
    let rec_tb = received_talkback_bytes.lock().unwrap().clone();
    assert_eq!(rec_tb.len(), 320);
    assert!(rec_tb.iter().all(|&b| b == 0xD5));

    // D. POST /api/front_door/audio/talkback with Content-Type: audio/pcm;base64
    received_talkback_bytes.lock().unwrap().clear();
    let b64_pcm = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 320]);
    let r_tb_b64 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/front_door/audio/talkback")
                .header(header::CONTENT_TYPE, "audio/pcm;base64")
                .body(Body::from(b64_pcm))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_tb_b64.status(), StatusCode::OK);
    let rec_tb_b64 = received_talkback_bytes.lock().unwrap().clone();
    assert_eq!(rec_tb_b64.len(), 160);
    assert!(rec_tb_b64.iter().all(|&b| b == 0xD5));

    // E. parse_onvif_profiles_and_video_sources_xml extracts ProfileToken and SourceToken
    let profiles_xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><trt:GetProfilesResponse xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><trt:Profiles token="Profile000" fixed="true"><tt:Name>MainStream</tt:Name><tt:VideoSourceConfiguration token="VSC000"><tt:SourceToken>VideoSource000</tt:SourceToken></tt:VideoSourceConfiguration></trt:Profiles></trt:GetProfilesResponse></s:Body></s:Envelope>"#;
    let (parsed_profs, parsed_vs) = parse_onvif_profiles_and_video_sources_xml(profiles_xml);
    assert_eq!(parsed_profs, vec!["Profile000".to_string()]);
    assert_eq!(parsed_vs, vec!["VideoSource000".to_string()]);

    srv_handle.abort();
}

#[tokio::test]
async fn async_export_jobs_and_timeline_preview_frames_enforce_scoping_and_lifecycle() {
    let ctx = setup_test_app().await;
    let state = ctx.state.clone();
    let auth_app = build_router_with_auth(state.clone(), true);
    let now = unix_now();
    let mint_jwt = |sub: &str, role: &str, cams: Option<&str>| -> String {
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: cams.map(str::to_string),
                iat: unix_now(),
                exp: now + 3600,
            },
            &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
        )
        .unwrap()
    };
    state
        .db
        .create_user_with_cameras("op_front", "pass1234", "operator", "front_door", 1000)
        .unwrap();
    let op_front_bearer = format!(
        "Bearer {}",
        mint_jwt("op_front", "operator", Some("front_door"))
    );
    let admin_bearer = format!("Bearer {}", mint_jwt("admin", "admin", Some("*")));

    // 1. Seed two recent preview frames in PreviewFrameStore for `front_door` and one for `backyard`
    let base_sec = now.saturating_sub(120);
    let t1_ms = base_sec * 1000 + 2_000;
    let t2_ms = base_sec * 1000 + 5_500;
    let t_back_ms = base_sec * 1000 + 3_000;
    let mut tiny_rgb = image::RgbImage::new(32, 18);
    for p in tiny_rgb.pixels_mut() {
        *p = image::Rgb([40, 120, 200]);
    }
    let mut jpeg_buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_buf, 75)
        .encode_image(&tiny_rgb)
        .unwrap();
    state
        .preview_frames
        .write_frame("front_door", t1_ms, &jpeg_buf)
        .unwrap();
    state
        .preview_frames
        .write_frame("front_door", t2_ms, &jpeg_buf)
        .unwrap();
    state
        .preview_frames
        .write_frame("backyard", t_back_ms, &jpeg_buf)
        .unwrap();

    // List frames for permitted camera -> returns the 2 indexed frames
    let r_list = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/preview/front_door/start/{base_sec}/end/{}/frames",
                    base_sec + 10
                ))
                .header(header::AUTHORIZATION, &op_front_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_list.status(), StatusCode::OK);
    let list_j: Value =
        serde_json::from_slice(&to_bytes(r_list.into_body(), 16384).await.unwrap()).unwrap();
    let frames_arr = list_j.as_array().unwrap();
    assert_eq!(frames_arr.len(), 2);
    assert_eq!(
        frames_arr[0]["url"],
        format!("/api/preview/front_door/frame/{t1_ms}.jpg")
    );

    // Fetch exact frame & nearest `/at/` frame -> 200 image/jpeg with x-frame-source: preview_store
    for ok_uri in [
        format!("/api/preview/front_door/frame/{t1_ms}.jpg"),
        format!("/api/preview/front_door/at/{}.jpg", t1_ms + 400),
        format!(
            "/api/front_door/frame?ts={:.3}",
            (t2_ms - 300) as f64 / 1000.0
        ),
    ] {
        let r_img = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&ok_uri)
                    .header(header::AUTHORIZATION, &op_front_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_img.status(), StatusCode::OK, "expected 200 on {ok_uri}");
        assert_eq!(
            r_img
                .headers()
                .get("x-frame-source")
                .and_then(|v| v.to_str().ok()),
            Some("preview_store")
        );
    }

    // Unrecorded gap returns 404 (never falls back to live camera snapshot!)
    let r_gap = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/preview/front_door/at/9000000.jpg")
                .header(header::AUTHORIZATION, &op_front_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_gap.status(), StatusCode::NOT_FOUND);

    // Non-permitted camera frame endpoints return 403 Forbidden
    for denied_uri in [
        format!("/api/preview/backyard/frame/{t_back_ms}.jpg"),
        format!("/api/preview/backyard/at/{t_back_ms}.jpg"),
        format!("/api/backyard/frame?ts={:.3}", t_back_ms as f64 / 1000.0),
    ] {
        let r_den = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&denied_uri)
                    .header(header::AUTHORIZATION, &op_front_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r_den.status(),
            StatusCode::FORBIDDEN,
            "expected 403 on {denied_uri}"
        );
    }

    // 2. Async export pre-flight rejects empty unrecorded range immediately with 400 BAD_REQUEST
    let r_async_empty = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/export/front_door/start/6000/end/6060")
                .header(header::AUTHORIZATION, &op_front_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Empty Range","async":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_async_empty.status(), StatusCode::BAD_REQUEST);

    // 3. Seed a cold-tier segment and run async export -> 202 Accepted -> completes in background
    let cold_root = state.media_dir.parent().unwrap().join("cold_tier_async");
    let cold_seg_dir = cold_root.join("2026-11-01/12/front_door");
    std::fs::create_dir_all(&cold_seg_dir).unwrap();
    {
        let mut cfg = state.config.write().await;
        cfg.record.storage_tiers.enabled = true;
        cfg.record.storage_tiers.cold_dir = Some(cold_root.clone());
    }
    let seg_file = cold_seg_dir.join("00.00.mp4");
    let mut stub_mp4 = vec![0u8; 2048];
    stub_mp4[0..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p']);
    std::fs::write(&seg_file, &stub_mp4).unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "async_cold_seg".to_string(),
            camera: "front_door".to_string(),
            path: seg_file.to_string_lossy().into_owned(),
            start_time: 7200.0,
            end_time: 7215.0,
            duration: 15.0,
            motion: 4,
            objects: 1,
            dbfs: 0,
            segment_size: 0.002,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();

    let r_async_ok = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/export/front_door/start/7200/end/7215")
                .header(header::AUTHORIZATION, &op_front_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"Async Cold Export","include_audio":true,"async":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_async_ok.status(), StatusCode::ACCEPTED);
    let async_j: Value =
        serde_json::from_slice(&to_bytes(r_async_ok.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(async_j["queued"], true);
    let exp_id = async_j["export_id"].as_str().unwrap().to_string();

    // Wait up to 3s for background completion
    let mut completed = false;
    for _ in 0..60 {
        let r_jobs = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/jobs/export")
                    .header(header::AUTHORIZATION, &op_front_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_jobs.status(), StatusCode::OK);
        let jobs_arr: Value =
            serde_json::from_slice(&to_bytes(r_jobs.into_body(), 16384).await.unwrap()).unwrap();
        if jobs_arr
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["id"] == exp_id && j["status"] == "completed")
        {
            completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(completed, "async export job must transition to completed");
    assert!(
        state
            .media_dir
            .join("exports")
            .join(format!("{exp_id}.mp4"))
            .exists()
    );

    // 4. Scoped operator cannot cancel an admin backyard job
    state.export_jobs.register_queued(
        "backyard_job_1".to_string(),
        "backyard".to_string(),
        "Backyard Job".to_string(),
        100.0,
        200.0,
        true,
        now as f64,
    );
    let r_cancel_denied = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/jobs/export/backyard_job_1/cancel")
                .header(header::AUTHORIZATION, &op_front_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_cancel_denied.status(), StatusCode::FORBIDDEN);

    let r_cancel_admin = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/jobs/export/backyard_job_1/cancel")
                .header(header::AUTHORIZATION, &admin_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_cancel_admin.status(), StatusCode::OK);
    assert_eq!(
        state.export_jobs.get("backyard_job_1").unwrap().status,
        export_jobs::ExportJobStatus::Cancelled
    );

    // 5. Mid-hour offline gap returns 404 even when an hourly preview row exists for that hour
    let prev_dir = state.media_dir.join("clips/previews/front_door");
    std::fs::create_dir_all(&prev_dir).unwrap();
    let gappy_prev = prev_dir.join("7200-10800.mp4");
    std::fs::write(&gappy_prev, vec![0u8; 256]).unwrap();
    state
        .db
        .insert_preview(
            "front_door_7200",
            "front_door",
            "/media/ninjanvr/clips/previews/front_door/7200-10800.mp4",
            7200.0,
            10800.0,
            600.0, // Only 10m recorded out of 60m hour -> gappy!
        )
        .unwrap();
    let r_mid_hour_gap = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/preview/front_door/at/9000000.jpg")
                .header(header::AUTHORIZATION, &op_front_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r_mid_hour_gap.status(),
        StatusCode::NOT_FOUND,
        "mid-hour unrecorded gap must return 404 even when a gappy hourly preview exists"
    );

    // 5b. Positive pruned continuous hourly preview returns 200 OK with x-frame-source: hourly_preview
    let cont_prev_mp4 = prev_dir.join("20000-20004.mp4");
    let ff_gen = tokio::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=navy:s=64x36:r=1:d=4",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&cont_prev_mp4)
        .status()
        .await;
    if ff_gen.is_ok_and(|s| s.success()) {
        state
            .db
            .insert_preview(
                "front_door_20000",
                "front_door",
                "/media/ninjanvr/clips/previews/front_door/20000-20004.mp4",
                20_000.0,
                20_004.0,
                4.0,
            )
            .unwrap();
        let r_cont_prev = auth_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/preview/front_door/at/20002000.jpg")
                    .header(header::AUTHORIZATION, &op_front_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r_cont_prev.status(), StatusCode::OK);
        assert_eq!(
            r_cont_prev
                .headers()
                .get("x-frame-source")
                .and_then(|v| v.to_str().ok()),
            Some("hourly_preview")
        );
    }

    // 6. Active export job queue cap returns 429 TOO_MANY_REQUESTS when full
    for i in 0..export_jobs::MAX_ACTIVE_EXPORT_JOBS {
        state
            .export_jobs
            .try_register_queued(
                format!("fill_{i}"),
                "front_door".to_string(),
                format!("Fill {i}"),
                7200.0,
                7215.0,
                true,
                now as f64,
            )
            .unwrap();
    }
    let r_queue_full = auth_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/export/front_door/start/7200/end/7215")
                .header(header::AUTHORIZATION, &op_front_bearer)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Overflow","async":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_queue_full.status(), StatusCode::TOO_MANY_REQUESTS);

    let _ = std::fs::remove_dir_all(&cold_root);
}

#[tokio::test]
async fn ux_remediation_bootstrap_offline_telemetry_tz_calendar_and_floorplan_work() {
    // 1. FloorplanRoom `name` alias & FloorplanWall `{x1, y1, x2, y2}` YAML deserialization
    let fp_yaml = r#"
version: "0.18-0"
ui:
  floorplan:
    width_m: 20.0
    height_m: 12.0
    rooms:
      - id: r1
        name: "Main Lobby"
        x: 0.1
        y: 0.1
        w: 0.4
        h: 0.4
    walls:
      - x1: 0.1
        y1: 0.1
        x2: 0.5
        y2: 0.1
        kind: wall
cameras: {}
"#;
    let fp_cfg = NinjaConfig::from_yaml_str(fp_yaml, PathBuf::from("/tmp/fp.yml")).unwrap();
    assert_eq!(fp_cfg.ui.floorplan.rooms[0].label, "Main Lobby");
    assert_eq!(fp_cfg.ui.floorplan.walls[0].points, [0.1, 0.1, 0.5, 0.1]);

    // 2. Honest offline telemetry (`0.0` FPS, `"offline"` quality, `x-ninjanvr-placeholder: 1`)
    let ctx = setup_test_app().await;
    let app = &ctx.app;
    let state = &ctx.state;

    let r_stats = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_stats.status(), StatusCode::OK);
    let stats_val: Value =
        serde_json::from_slice(&to_bytes(r_stats.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(stats_val["cameras"]["front_door"]["camera_fps"], 0.0);
    assert_eq!(stats_val["cameras"]["front_door"]["ffmpeg_cpu"], "0.0");
    assert_eq!(
        stats_val["cameras"]["front_door"]["connection_quality"],
        "offline"
    );

    let r_placeholder = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/front_door/latest.jpg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_placeholder.status(), StatusCode::OK);
    assert_eq!(
        r_placeholder
            .headers()
            .get("x-ninjanvr-placeholder")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );

    // 3. Heuristic shortfall vs asymmetry split (uniform 14d retention must not claim "varies from 14d to 14d")
    let uniform_yaml = r#"
version: "0.18-0"
cameras:
  cam_1:
    enabled: true
    record:
      enabled: true
      continuous:
        days: 14
  cam_2:
    enabled: true
    record:
      enabled: true
      continuous:
        days: 14
"#;
    let uni_cfg = NinjaConfig::from_yaml_str(uniform_yaml, PathBuf::from("/tmp/uni.yml")).unwrap();
    let uni_mount = json!({ "total": 10_000.0, "used": 8_000.0, "free": 2_000.0 });
    let uni_alerts = evaluate_system_heuristics(&uni_cfg, &serde_json::Map::new(), &uni_mount, 1.5);
    assert!(
        uni_alerts
            .iter()
            .any(|a| a.id == "retention_runway_shortfall"),
        "uniform retention exceeding disk runway must emit retention_runway_shortfall"
    );
    assert!(
        !uni_alerts
            .iter()
            .any(|a| a.id == "retention_runway_asymmetry"),
        "uniform retention must not emit retention_runway_asymmetry"
    );

    // 4. Timezone-bucketed multi-camera `/api/recordings/days?tz_offset_min=180` (BRT UTC-3)
    // Pick a recent UTC midnight + 01:30:00 (which is 22:30:00 on the PREVIOUS local day in UTC-3!)
    let now_sec = unix_now() as i64;
    let recent_utc_midnight = (now_sec / 86400) * 86400;
    let utc_0130 = recent_utc_midnight + 5400; // 01:30 UTC today -> 22:30 BRT yesterday
    let expected_brt_day = chrono::DateTime::from_timestamp(utc_0130 - 180 * 60, 0)
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    let expected_utc_day = chrono::DateTime::from_timestamp(utc_0130, 0)
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    assert_ne!(expected_brt_day, expected_utc_day);

    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "tz_seg_brt".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/tz_seg_brt.mp4".to_string(),
            start_time: utc_0130 as f64,
            end_time: (utc_0130 + 60) as f64,
            duration: 60.0,
            motion: 3,
            objects: 1,
            dbfs: 0,
            segment_size: 1.0,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();

    let r_days_brt = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/recordings/days?tz_offset_min=180")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_days_brt.status(), StatusCode::OK);
    let days_brt: Value =
        serde_json::from_slice(&to_bytes(r_days_brt.into_body(), 16384).await.unwrap()).unwrap();
    assert!(
        days_brt
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["day"] == expected_brt_day),
        "BRT offset (+180) must bucket 01:30 UTC into previous local day {expected_brt_day}"
    );

    // Non-integer timezone (`UTC+5:30` -> `tz_offset_min=-330`): two segments at 18:15 UTC (23:45 IST) and 18:45 UTC (00:15 IST next day)
    // share the same UTC hour `18:00..19:00` and must split cleanly into two distinct IST days via 15-min buckets!
    let utc_1815 = recent_utc_midnight - 86400 + 18 * 3600 + 15 * 60;
    let utc_1845 = recent_utc_midnight - 86400 + 18 * 3600 + 45 * 60;
    let expected_ist_day_1 = chrono::DateTime::from_timestamp(utc_1815 + 330 * 60, 0)
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    let expected_ist_day_2 = chrono::DateTime::from_timestamp(utc_1845 + 330 * 60, 0)
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    assert_ne!(expected_ist_day_1, expected_ist_day_2);
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "tz_seg_ist_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/tz_seg_ist_1.mp4".to_string(),
            start_time: utc_1815 as f64,
            end_time: (utc_1815 + 60) as f64,
            duration: 60.0,
            motion: 5,
            objects: 1,
            dbfs: -22,
            segment_size: 1.0,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();
    state
        .db
        .insert_recording(&ninjanvr_db::RecordingRow {
            id: "tz_seg_ist_2".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/tz_seg_ist_2.mp4".to_string(),
            start_time: utc_1845 as f64,
            end_time: (utc_1845 + 60) as f64,
            duration: 60.0,
            motion: 5,
            objects: 1,
            dbfs: -22,
            segment_size: 1.0,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();
    let r_days_ist = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/recordings/days?tz_offset_min=-330")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_days_ist.status(), StatusCode::OK);
    let days_ist: Value =
        serde_json::from_slice(&to_bytes(r_days_ist.into_body(), 32768).await.unwrap()).unwrap();
    let ist_arr = days_ist.as_array().unwrap();
    assert!(
        ist_arr.iter().any(|d| d["day"] == expected_ist_day_1)
            && ist_arr.iter().any(|d| d["day"] == expected_ist_day_2),
        "IST offset (-330) must split 18:15 UTC and 18:45 UTC across local midnight into {expected_ist_day_1} and {expected_ist_day_2}"
    );

    // 5. Initial admin bootstrap file (`0600`) + `force_reset` + `/api/auth/providers` authenticated status
    let reset_pw = state
        .db
        .ensure_default_admin_with_options(10_000, None, true)
        .unwrap()
        .expect("force_reset without explicit password must generate a new secret");
    assert_eq!(reset_pw.len(), 32);
    let cfg_dir = ctx.tmp_dir.join("cfg_bootstrap_test");
    let written_pw_file =
        write_initial_admin_password_file(&cfg_dir, &reset_pw).expect("must write 0600 secret");
    assert_eq!(
        std::fs::read_to_string(&written_pw_file).unwrap().trim(),
        reset_pw
    );
    remove_initial_admin_password_file(&cfg_dir);
    assert!(!written_pw_file.exists());

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
    let prov_j: Value =
        serde_json::from_slice(&to_bytes(r_prov.into_body(), 4096).await.unwrap()).unwrap();
    assert!(
        prov_j
            .get("authenticated")
            .and_then(Value::as_bool)
            .is_some()
    );
    assert!(
        prov_j
            .get("admin_first_time_login")
            .and_then(Value::as_bool)
            .is_some()
    );

    // 6. Case create + delete (`DELETE /api/cases/{id}`) & one-shot `reset_admin_password` clearing
    let r_case_post = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/cases")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"id":"case_ux_test","name":"UX Audit Case","event_ids":[],"export_ids":[]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_case_post.status(), StatusCode::OK);
    let case_post_j: Value =
        serde_json::from_slice(&to_bytes(r_case_post.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(case_post_j["case"]["name"], "UX Audit Case");

    let r_case_del = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/cases/case_ux_test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_case_del.status(), StatusCode::OK);
    assert!(state.db.get_case("case_ux_test").unwrap().is_none());

    // Verify `reset_admin_password: true` via `/api/config/set` structured patch resets once and clears flag in response + disk
    let r_reset_patch = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/config/set")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"auth":{"reset_admin_password":true}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r_reset_patch.status(), StatusCode::OK);
    let reset_patch_j: Value =
        serde_json::from_slice(&to_bytes(r_reset_patch.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(
        reset_patch_j["config"]["auth"]["reset_admin_password"],
        false
    );
    assert!(!state.config.read().await.auth.reset_admin_password);
}
