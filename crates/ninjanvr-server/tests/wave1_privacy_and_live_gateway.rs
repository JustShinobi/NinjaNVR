use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use image::{Rgb, RgbImage};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use ninjanvr_server::mse_hub::{MseClientInFrame, MseClientOutFrame, MseStreamHub};
use ninjanvr_server::pipeline::normalize_privacy_mask_for_resolution;
use ninjanvr_server::{
    InMemoryGo2RtcUpstream, JwtClaims, LiveStreamGateway, build_router_with_auth, new_app_state,
    normalize_privacy_mask_value_for_resolution,
};
use ninjanvr_vision::PrivacyMaskPolicy;
use serde_json::{Value, json};
use tower::ServiceExt;

fn mint_jwt(secret: &str, sub: &str, role: &str, allowed_cameras: &str) -> String {
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

fn make_iso_box(box_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let total = (8 + payload.len()) as u32;
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(box_type);
    out.extend_from_slice(payload);
    out
}

fn make_iso_full_box(box_type: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let total = (12 + body.len()) as u32;
    let mut out = Vec::with_capacity(12 + body.len());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(box_type);
    let vf = ((version as u32) << 24) | (flags & 0x00FF_FFFF);
    out.extend_from_slice(&vf.to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn build_placeholder_128x96_init_segment() -> Bytes {
    let ftyp = make_iso_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso6dashmp41");

    // Version 0 tkhd (84-byte payload) with 128x96 fixed-point dimensions at bytes [76..84]
    let mut tkhd_body = vec![0u8; 80];
    tkhd_body[72..76].copy_from_slice(&(128u32 << 16).to_be_bytes());
    tkhd_body[76..80].copy_from_slice(&(96u32 << 16).to_be_bytes());
    let tkhd = make_iso_full_box(b"tkhd", 0, 3, &tkhd_body);

    let mut hdlr_body = vec![0u8; 20];
    hdlr_body[4..8].copy_from_slice(b"vide");
    let hdlr = make_iso_full_box(b"hdlr", 0, 0, &hdlr_body);

    // avc1 visual sample entry with 128x96 at payload[24..28] + avcC carrying GO2RTC_PLACEHOLDER_H264_SPS
    let mut avcc_payload = vec![0x01, 0x42, 0x00, 0x0a, 0xff, 0xe1, 0x00, 0x07];
    avcc_payload.extend_from_slice(&ninjanvr_vod::GO2RTC_PLACEHOLDER_H264_SPS);
    avcc_payload.extend_from_slice(&[0x01, 0x00, 0x04, 0x68, 0xce, 0x3c, 0x80]);
    let avcc = make_iso_box(b"avcC", &avcc_payload);

    let mut avc1_payload = vec![0u8; 78];
    avc1_payload[24..26].copy_from_slice(&128u16.to_be_bytes());
    avc1_payload[26..28].copy_from_slice(&96u16.to_be_bytes());
    avc1_payload.extend_from_slice(&avcc);
    let avc1 = make_iso_box(b"avc1", &avc1_payload);

    let mut stsd_body = Vec::new();
    stsd_body.extend_from_slice(&1u32.to_be_bytes());
    stsd_body.extend_from_slice(&avc1);
    let stsd = make_iso_full_box(b"stsd", 0, 0, &stsd_body);

    let stbl = make_iso_box(b"stbl", &stsd);
    let minf = make_iso_box(b"minf", &stbl);
    let mut mdia_children = Vec::new();
    mdia_children.extend_from_slice(&hdlr);
    mdia_children.extend_from_slice(&minf);
    let mdia = make_iso_box(b"mdia", &mdia_children);

    let mut trak_children = Vec::new();
    trak_children.extend_from_slice(&tkhd);
    trak_children.extend_from_slice(&mdia);
    let trak = make_iso_box(b"trak", &trak_children);
    let moov = make_iso_box(b"moov", &trak);

    let mut out = Vec::with_capacity(ftyp.len() + moov.len());
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    Bytes::from(out)
}

fn build_h264_fragment(is_sync: bool, mdat_payload: &[u8]) -> Bytes {
    let sample_flags: u32 = if is_sync { 0x0200_0000 } else { 0x0101_0000 };
    let mfhd = make_iso_full_box(b"mfhd", 0, 0, &1u32.to_be_bytes());
    let tfhd = make_iso_full_box(b"tfhd", 0, 0x020000, &1u32.to_be_bytes());
    let mut trun_body = Vec::new();
    trun_body.extend_from_slice(&1u32.to_be_bytes());
    trun_body.extend_from_slice(&64i32.to_be_bytes());
    trun_body.extend_from_slice(&3000u32.to_be_bytes());
    trun_body.extend_from_slice(&(mdat_payload.len() as u32).to_be_bytes());
    trun_body.extend_from_slice(&sample_flags.to_be_bytes());
    let trun = make_iso_full_box(b"trun", 0, 0x000701, &trun_body);
    let mut traf_children = Vec::new();
    traf_children.extend_from_slice(&tfhd);
    traf_children.extend_from_slice(&trun);
    let traf = make_iso_box(b"traf", &traf_children);
    let mut moof_children = Vec::new();
    moof_children.extend_from_slice(&mfhd);
    moof_children.extend_from_slice(&traf);
    let moof = make_iso_box(b"moof", &moof_children);
    let mdat = make_iso_box(b"mdat", mdat_payload);
    let mut out = Vec::with_capacity(moof.len() + mdat.len());
    out.extend_from_slice(&moof);
    out.extend_from_slice(&mdat);
    Bytes::from(out)
}

#[tokio::test]
async fn wave1_privacy_mask_2pt_parity_lpr_simulate_redaction_and_fail_closed_gating() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-wave1-priv-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let media_dir = tmp_dir.join("media");
    let clips_dir = media_dir.join("clips");
    std::fs::create_dir_all(&clips_dir).unwrap();
    let web_dir = tmp_dir.join("web");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");

    // Configure front_door with a 2-point pixel box ("100,100,400,300" on 640x360)
    // and broken_cam with a malformed mask
    let yaml = format!(
        r#"version: 0.18-0
database:
  path: {}
auth:
  enabled: true
cameras:
  front_door:
    enabled: true
    detect:
      width: 640
      height: 360
    privacy_mask: "100,100,400,300"
  broken_cam:
    enabled: true
    detect:
      width: 640
      height: 360
    privacy_mask:
      - "corrupt_polygon_spec"
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    // 1. Exact parity between pipeline::normalize_privacy_mask_for_resolution,
    //    share_engine::normalize_privacy_mask_value_for_resolution, and PrivacyMaskPolicy
    for mask_input in [
        json!("100,100,400,300"),
        json!([100, 100, 400, 300]),
        json!([[100, 100], [400, 100], [400, 300], [100, 300]]),
    ] {
        let pipe_norm = normalize_privacy_mask_for_resolution(&mask_input, 640, 360);
        let share_norm = normalize_privacy_mask_value_for_resolution(&mask_input, 640, 360);
        let policy = PrivacyMaskPolicy::from_value(&mask_input, 640, 360);
        assert_eq!(pipe_norm, share_norm, "mask={mask_input}");
        assert_eq!(pipe_norm, policy.to_normalized_value(), "mask={mask_input}");
        assert!(policy.ffmpeg_drawbox_filter().unwrap().is_some());
    }

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let app = build_router_with_auth(state.clone(), true);
    let admin_bearer = format!(
        "Bearer {}",
        mint_jwt(&state.jwt_secret, "admin", "admin", "*")
    );

    // Seed bright 640x360 latest frames on disk for both cameras
    let bright_rgb = RgbImage::from_pixel(640, 360, Rgb([235, 235, 235]));
    let bright_jpg = ninjanvr_vision::encode_image_jpeg(&bright_rgb).unwrap();
    std::fs::write(clips_dir.join("latest_front_door.jpg"), &bright_jpg).unwrap();
    std::fs::write(clips_dir.join("latest_broken_cam.jpg"), &bright_jpg).unwrap();

    // 2. HTTP /api/front_door/latest.jpg redacts the 2-point "100,100,400,300" box while leaving exterior bright
    let fd_latest = app
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
    assert_eq!(fd_latest.status(), StatusCode::OK);
    let fd_bytes = to_bytes(fd_latest.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let fd_img = image::load_from_memory(&fd_bytes).unwrap().to_rgb8();
    assert!(
        fd_img.get_pixel(250, 200)[0] < 30,
        "interior pixel (250,200) inside 2-point box 100,100,400,300 must be redacted"
    );
    assert!(
        fd_img.get_pixel(40, 40)[0] > 200,
        "exterior pixel (40,40) outside 2-point box must remain bright"
    );

    // 3. POST /api/debug/lpr-simulate on front_door redacts all written snapshot files and base64 thumbnail!
    let sim_fd_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("http://localhost/api/debug/lpr-simulate")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::ORIGIN, "http://localhost")
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"camera":"front_door","plate":"BRA2E19"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sim_fd_resp.status(), StatusCode::OK);
    let sim_fd_json: Value =
        serde_json::from_slice(&to_bytes(sim_fd_resp.into_body(), 512 * 1024).await.unwrap())
            .unwrap();
    let ev_id = sim_fd_json["event_id"].as_str().unwrap();
    let rev_id = sim_fd_json["review_id"].as_str().unwrap();

    for disk_snap in [
        clips_dir.join(format!("front_door-{ev_id}-clean.webp")),
        clips_dir.join(format!("{ev_id}.jpg")),
        clips_dir.join(format!("front_door-{ev_id}.webp")),
    ] {
        assert!(
            disk_snap.is_file(),
            "expected simulated snapshot {}",
            disk_snap.display()
        );
        let snap_img = image::open(&disk_snap).unwrap().to_rgb8();
        let masked_px = snap_img.get_pixel(250, 240);
        assert!(
            masked_px[0] < 30 && masked_px[1] < 30 && masked_px[2] < 35,
            "simulated LPR artifact {} must redact masked plate/vehicle interior, got {masked_px:?}",
            disk_snap.display()
        );
    }
    let rev_thumb_path = clips_dir
        .join("review")
        .join(format!("thumb-front_door-{rev_id}.webp"));
    assert!(rev_thumb_path.is_file());
    let rev_img = image::open(&rev_thumb_path).unwrap().to_rgb8();
    assert!(
        rev_img.get_pixel(125, 120)[0] < 30,
        "downscaled 180p simulated review thumbnail must also be redacted"
    );
    let stored_ev = state.db.get_event(ev_id).unwrap().unwrap();
    let b64_thumb = stored_ev["thumbnail"].as_str().unwrap();
    let thumb_bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_thumb)
        .unwrap();
    let thumb_img = image::load_from_memory(&thumb_bytes).unwrap().to_rgb8();
    assert!(thumb_img.get_pixel(125, 120)[0] < 30);

    // 4. Malformed mask on broken_cam fails closed:
    //    - POST /api/debug/lpr-simulate blocks writing unredacted snapshot files & omits thumbnail
    //    - /vod/broken_cam/... and /clips/latest_broken_cam.jpg return 404 NOT_FOUND
    let sim_broken_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("http://localhost/api/debug/lpr-simulate")
                .header(header::AUTHORIZATION, &admin_bearer)
                .header(header::ORIGIN, "http://localhost")
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"camera":"broken_cam","plate":"RIO4A88"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sim_broken_resp.status(), StatusCode::OK);
    let sim_broken_json: Value = serde_json::from_slice(
        &to_bytes(sim_broken_resp.into_body(), 512 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    let broken_ev_id = sim_broken_json["event_id"].as_str().unwrap();
    assert!(
        !clips_dir
            .join(format!("broken_cam-{broken_ev_id}-clean.webp"))
            .exists(),
        "malformed fail-closed mask must block writing unredacted clean snapshot"
    );
    assert!(
        !clips_dir.join(format!("{broken_ev_id}.jpg")).exists(),
        "malformed fail-closed mask must block writing unredacted JPEG snapshot"
    );
    let broken_ev = state.db.get_event(broken_ev_id).unwrap().unwrap();
    assert!(
        broken_ev["thumbnail"].is_null(),
        "malformed fail-closed mask must not persist unredacted base64 thumbnail"
    );

    for blocked_uri in [
        "/vod/broken_cam/start/100/end/120/master.m3u8",
        "/clips/latest_broken_cam.jpg",
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(blocked_uri)
                    .header(header::AUTHORIZATION, &admin_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::NOT_FOUND,
            "malformed mask must fail closed with 404 on {blocked_uri}"
        );
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn wave1_live_gateway_scoped_talkback_rbac_mse_pump_sps_patch_and_config_invalidation() {
    let tmp_dir =
        std::env::temp_dir().join(format!("ninjanvr-wave1-live-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&media_dir).unwrap();
    let web_dir = tmp_dir.join("web");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();
    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let go2rtc_out = tmp_dir.join("go2rtc.yaml");

    let yaml = format!(
        r#"version: 0.18-0
database:
  path: {}
auth:
  enabled: true
go2rtc:
  streams:
    front_door:
      - "rtsp://camuser:secretpass@192.0.2.10:554/stream1"
      - "tapo://cloudpw@192.0.2.10"
    front_door_sub:
      - "rtsp://camuser:secretpass@192.0.2.10:554/stream2"
      - "tapo://cloudpw@192.0.2.10"
    backyard:
      - "rtsp://camuser:secretpass@192.0.2.11:554/stream1"
      - "tapo://cloudpw@192.0.2.11"
cameras:
  front_door:
    enabled: true
  backyard:
    enabled: true
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let mut state = new_app_state(&cfg_path, web_dir, media_dir, false)
        .await
        .unwrap();
    state.config_coordinator = state
        .config_coordinator
        .clone()
        .with_go2rtc_config_path(&go2rtc_out);

    // Inject InMemoryGo2RtcUpstream into state.live_gateway for deterministic end-to-end Axum route + auth_gate testing
    let fake_upstream = Arc::new(InMemoryGo2RtcUpstream::with_streams(json!({
        "front_door": { "producers": [{ "url": "rtsp://camuser:secretpass@192.0.2.10:554/stream1" }] },
        "front_door_sub": { "producers": [{ "url": "rtsp://camuser:secretpass@192.0.2.10:554/stream2" }] },
        "front_door_talk": { "producers": [{ "url": "tapo://cloudpw@192.0.2.10" }] },
        "front_door_sub_talk": { "producers": [{ "url": "tapo://cloudpw@192.0.2.10" }] },
        "backyard": { "producers": [{ "url": "rtsp://camuser:secretpass@192.0.2.11:554/stream1" }] },
        "backyard_talk": { "producers": [{ "url": "tapo://cloudpw@192.0.2.11" }] },
        "backyard_sub_talk": { "producers": [{ "url": "tapo://cloudpw@192.0.2.11" }] },
    })));
    state.live_gateway = LiveStreamGateway::new(
        state.config.clone(),
        state.mse_hub.clone(),
        fake_upstream.clone(),
    );

    state
        .db
        .create_user_with_cameras("op_front", "TestPass1234!", "operator", "front_door", 1000)
        .unwrap();
    let app = build_router_with_auth(state.clone(), true);
    let scoped_operator_bearer = format!(
        "Bearer {}",
        mint_jwt(&state.jwt_secret, "op_front", "operator", "front_door")
    );

    // 1. End-to-end Axum + auth_gate_middleware RBAC for _talk and _sub_talk streams
    let streams_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/go2rtc/streams")
                .header(header::AUTHORIZATION, &scoped_operator_bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(streams_resp.status(), StatusCode::OK);
    let streams_json: Value = serde_json::from_slice(
        &to_bytes(streams_resp.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    let streams_obj = streams_json.as_object().unwrap();
    for expected in [
        "front_door",
        "front_door_sub",
        "front_door_talk",
        "front_door_sub_talk",
    ] {
        assert!(streams_obj.contains_key(expected), "missing {expected}");
    }
    assert!(!streams_obj.contains_key("backyard"));
    assert!(!streams_obj.contains_key("backyard_talk"));
    assert!(!streams_obj.contains_key("backyard_sub_talk"));

    for ok_stream in ["front_door_talk", "front_door_sub_talk"] {
        let get_one = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/go2rtc/streams/{ok_stream}"))
                    .header(header::AUTHORIZATION, &scoped_operator_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get_one.status(), StatusCode::OK, "GET stream {ok_stream}");

        let prewarm_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "http://localhost/api/go2rtc/streams/{ok_stream}/prewarm"
                    ))
                    .header(header::AUTHORIZATION, &scoped_operator_bearer)
                    .header(header::ORIGIN, "http://localhost")
                    .header(header::HOST, "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            prewarm_resp.status(),
            StatusCode::OK,
            "POST prewarm {ok_stream}"
        );
    }

    for denied_stream in ["backyard_talk", "backyard_sub_talk"] {
        let denied_get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/go2rtc/streams/{denied_stream}"))
                    .header(header::AUTHORIZATION, &scoped_operator_bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied_get.status(), StatusCode::FORBIDDEN);

        let denied_prewarm = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "http://localhost/api/go2rtc/streams/{denied_stream}/prewarm"
                    ))
                    .header(header::AUTHORIZATION, &scoped_operator_bearer)
                    .header(header::ORIGIN, "http://localhost")
                    .header(header::HOST, "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied_prewarm.status(), StatusCode::FORBIDDEN);
    }

    // Talkback SDP offer via auth_gate + LiveStreamGateway on src=front_door_sub & src=front_door_sub_talk
    let talk_sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\n";
    for query in ["src=front_door_sub", "src=front_door_sub_talk"] {
        let wr_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("http://localhost/api/go2rtc/webrtc?{query}"))
                    .header(header::AUTHORIZATION, &scoped_operator_bearer)
                    .header(header::ORIGIN, "http://localhost")
                    .header(header::HOST, "localhost")
                    .header(header::CONTENT_TYPE, "application/sdp")
                    .body(Body::from(talk_sdp))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wr_resp.status(), StatusCode::OK, "webrtc query={query}");
    }
    let denied_wr = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("http://localhost/api/go2rtc/webrtc?src=backyard_sub_talk")
                .header(header::AUTHORIZATION, &scoped_operator_bearer)
                .header(header::ORIGIN, "http://localhost")
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/sdp")
                .body(Body::from(talk_sdp))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied_wr.status(), StatusCode::FORBIDDEN);

    let recorded_webrtc = fake_upstream
        .captured_webrtc_queries
        .lock()
        .unwrap()
        .clone();
    assert_eq!(recorded_webrtc.len(), 2);
    assert_eq!(recorded_webrtc[0].0, "src=front_door_talk");
    assert_eq!(recorded_webrtc[1].0, "src=front_door_sub_talk");

    // 2. MseStreamHub 128x96 SPS init patching + pump_client_session + ConfigCoordinator invalidation
    let hub: &MseStreamHub = &state.mse_hub;
    let codec = r#"{"type":"mse","value":"video/mp4; codecs=\"avc1.640029\""}"#;
    hub.ingest_upstream_text("front_door_sub", codec).await;
    hub.ingest_upstream_binary("front_door_sub", build_placeholder_128x96_init_segment())
        .await;

    // 1920x1080 H.264 High Profile SPS NAL inside 4-byte AVCC length-prefixed sync fragment
    let sps_1080p: [u8; 23] = [
        0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00,
        0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf0,
    ];
    let mut avcc_mdat = Vec::new();
    avcc_mdat.extend_from_slice(&(sps_1080p.len() as u32).to_be_bytes());
    avcc_mdat.extend_from_slice(&sps_1080p);
    let sync_1080p = build_h264_fragment(true, &avcc_mdat);
    assert!(
        hub.ingest_upstream_binary("front_door_sub", sync_1080p)
            .await
    );

    // Cached burst init segment must now have its 128x96 placeholder dimensions patched to 1920x1080!
    let burst = hub
        .get_fresh_burst("front_door_sub")
        .await
        .expect("fresh burst");
    assert!(
        !ninjanvr_vod::fmp4_init_has_placeholder_video_dimensions(&burst.init_segment),
        "128x96 placeholder init segment must be patched to 1920x1080 upon 1080p SPS keyframe"
    );

    // Warm backyard_sub too so we verify targeted per-camera invalidation
    hub.ingest_upstream_text("backyard_sub", codec).await;
    hub.ingest_upstream_binary("backyard_sub", build_placeholder_128x96_init_segment())
        .await;
    hub.ingest_upstream_binary("backyard_sub", build_h264_fragment(true, &[0x55; 32]))
        .await;
    assert!(hub.has_active_or_cached_stream("backyard_sub").await);

    // Pump a simulated MSE client session over in-memory channels
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<MseClientOutFrame>();
    let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel::<Result<MseClientInFrame, ()>>();
    let sink = futures_util::sink::unfold(out_tx, |tx, frame: MseClientOutFrame| async move {
        let _ = tx.send(frame);
        Ok::<_, ()>(tx)
    });
    let stream_in = futures_util::stream::unfold(in_rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });

    let hub_clone = hub.clone();
    let pump_handle = tokio::spawn(async move {
        hub_clone
            .pump_client_session(
                Box::pin(sink),
                Box::pin(stream_in),
                "ws://127.0.0.1:9",
                "front_door_sub",
            )
            .await;
    });

    // Send initial client Ping and verify Pong + Codec + patched Init + Keyframe burst arrive
    in_tx
        .send(Ok(MseClientInFrame::Ping(Bytes::from_static(b"hb"))))
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(1), out_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first, MseClientOutFrame::Pong(Bytes::from_static(b"hb")));
    let second = tokio::time::timeout(Duration::from_secs(1), out_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second, MseClientOutFrame::Text(codec.to_string()));
    let third = tokio::time::timeout(Duration::from_secs(1), out_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(third, MseClientOutFrame::Binary(_)));
    let fourth = tokio::time::timeout(Duration::from_secs(1), out_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(fourth, MseClientOutFrame::Binary(_)));

    // Modifying front_door's go2rtc stream URL via ConfigCoordinator invalidates front_door* MSE sessions
    // (closing pump_client_session cleanly) while preserving backyard_sub!
    state
        .config_coordinator
        .commit_structured_patch(
            &json!({
                "go2rtc": {
                    "streams": {
                        "front_door": ["rtsp://camuser:secretpass@192.0.2.99:554/stream1"],
                        "front_door_sub": ["rtsp://camuser:secretpass@192.0.2.99:554/stream2"],
                        "backyard": [
                            "rtsp://camuser:secretpass@192.0.2.11:554/stream1",
                            "tapo://cloudpw@192.0.2.11"
                        ]
                    }
                }
            }),
            None,
            Some(&state.camera_driver_runtime),
        )
        .await
        .expect("structured stream update should commit");

    assert!(
        !hub.has_active_or_cached_stream("front_door_sub").await,
        "ConfigCoordinator stream update must invalidate front_door_sub MSE cache & session"
    );
    assert!(
        hub.has_active_or_cached_stream("backyard_sub").await,
        "unchanged backyard_sub MSE cache must remain warm"
    );

    tokio::time::timeout(Duration::from_secs(1), pump_handle)
        .await
        .expect("pump_client_session must exit promptly when camera stream is invalidated")
        .expect("pump_client_session task must not panic");

    drop(in_tx);
    let _ = std::fs::remove_dir_all(&tmp_dir);
}
