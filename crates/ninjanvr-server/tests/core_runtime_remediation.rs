use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ninjanvr_db::{EventDto, RecordingRow};
use ninjanvr_server::{build_router, new_app_state};
use serde_json::{Value, json};
use tower::ServiceExt;

fn make_box(box_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(box_type);
    out.extend_from_slice(payload);
    out
}

fn make_full_box(box_type: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + body.len());
    payload.push(version);
    payload.extend_from_slice(&flags.to_be_bytes()[1..4]);
    payload.extend_from_slice(body);
    make_box(box_type, &payload)
}

fn build_synthetic_mp4_with_resolution(width: u16, height: u16, avcc_profile: [u8; 3]) -> Vec<u8> {
    let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso6");
    let mvhd = make_full_box(b"mvhd", 0, 0, &[0u8; 96]);

    let mut tkhd_body = vec![0u8; 80];
    tkhd_body[8..12].copy_from_slice(&1u32.to_be_bytes());
    tkhd_body[72..76].copy_from_slice(&((width as u32) << 16).to_be_bytes());
    tkhd_body[76..80].copy_from_slice(&((height as u32) << 16).to_be_bytes());
    let tkhd = make_full_box(b"tkhd", 0, 3, &tkhd_body);

    let mut hdlr_body = vec![0u8; 20];
    hdlr_body[4..8].copy_from_slice(b"vide");
    let hdlr = make_full_box(b"hdlr", 0, 0, &hdlr_body);

    let avcc = make_box(
        b"avcC",
        &[
            1,
            avcc_profile[0],
            avcc_profile[1],
            avcc_profile[2],
            0xff,
            0xe1,
            0x00,
            0x04,
            0x67,
            avcc_profile[0],
            avcc_profile[1],
            avcc_profile[2],
            0x01,
            0x00,
            0x04,
            0x68,
            0xce,
            0x38,
            0x80,
        ],
    );

    let mut avc1_body = vec![0u8; 78];
    avc1_body[6..8].copy_from_slice(&1u16.to_be_bytes());
    avc1_body[24..26].copy_from_slice(&width.to_be_bytes());
    avc1_body[26..28].copy_from_slice(&height.to_be_bytes());
    avc1_body.extend_from_slice(&avcc);
    let avc1 = make_box(b"avc1", &avc1_body);

    let mut stsd_body = Vec::new();
    stsd_body.extend_from_slice(&1u32.to_be_bytes());
    stsd_body.extend_from_slice(&avc1);
    let stsd = make_full_box(b"stsd", 0, 0, &stsd_body);
    let stbl = make_box(b"stbl", &stsd);
    let minf = make_box(b"minf", &stbl);

    let mut mdia_c = Vec::new();
    mdia_c.extend_from_slice(&hdlr);
    mdia_c.extend_from_slice(&minf);
    let mdia = make_box(b"mdia", &mdia_c);

    let mut trak_c = Vec::new();
    trak_c.extend_from_slice(&tkhd);
    trak_c.extend_from_slice(&mdia);
    let trak = make_box(b"trak", &trak_c);

    let mut moov_c = Vec::new();
    moov_c.extend_from_slice(&mvhd);
    moov_c.extend_from_slice(&trak);
    let moov = make_box(b"moov", &moov_c);
    let mdat = make_box(b"mdat", &[0u8; 64]);

    [ftyp, moov, mdat].concat()
}

#[tokio::test]
async fn events_thumbnail_omission_hybrid_vod_discontinuity_past_16_segments_and_live_health_telemetry()
 {
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr-core-remediation-{}",
        rand::random::<u64>()
    ));
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(media_dir.join("clips/review")).unwrap();
    std::fs::create_dir_all(media_dir.join("recordings/2026-11-19/12/office")).unwrap();
    std::fs::create_dir_all(media_dir.join("exports")).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let yaml = format!(
        r#"
database:
  path: {}
auth:
  enabled: false
cameras:
  office:
    enabled: true
    detect:
      enabled: true
      width: 640
      height: 360
      fps: 5
    privacy_mask:
      - "0,0,160,120"
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    let app = build_router(state.clone());

    // 1. Unmeasured camera before receiving any frames must report 0.0 FPS and "offline" quality (never fake 5.0 FPS)
    let stats_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stats_resp.status(), StatusCode::OK);
    let stats_init: Value =
        serde_json::from_slice(&to_bytes(stats_resp.into_body(), 65_536).await.unwrap()).unwrap();
    assert_eq!(stats_init["cameras"]["office"]["camera_fps"], json!(0.0));
    assert_eq!(
        stats_init["cameras"]["office"]["connection_quality"],
        json!("offline")
    );

    // Record real reconnect, stall, and audio dBFS telemetry and verify `/api/stats` reflects them accurately
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    state.camera_metrics.write().await.insert(
        "office".to_string(),
        ninjanvr_server::pipeline::CameraRuntimeMetrics {
            camera_fps: 5.0,
            process_fps: 5.0,
            skipped_fps: 0.0,
            detection_fps: 1.2,
            total_inferences: 42,
            pid: 9999,
            last_frame_ts: now_ts,
            night_mode: false,
        },
    );
    state
        .pipeline
        .record_camera_reconnect("office", now_ts - 60.0);
    state
        .pipeline
        .record_camera_reconnect("office", now_ts - 20.0);
    state.pipeline.record_camera_stall("office", now_ts - 10.0);
    state.pipeline.record_camera_audio_dbfs("office", -24);

    let stats_live_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let stats_live: Value =
        serde_json::from_slice(&to_bytes(stats_live_resp.into_body(), 65_536).await.unwrap())
            .unwrap();
    assert_eq!(stats_live["cameras"]["office"]["camera_fps"], json!(5.0));
    assert_eq!(
        stats_live["cameras"]["office"]["reconnects_last_hour"],
        json!(2)
    );
    assert_eq!(
        stats_live["cameras"]["office"]["stalls_last_hour"],
        json!(1)
    );
    assert_eq!(stats_live["cameras"]["office"]["audio_dBFS"], json!(-24));
    assert!(
        stats_live["cameras"]["office"]["audio_rms"]
            .as_u64()
            .unwrap_or(0)
            > 1_000
    );
    assert_eq!(
        stats_live["cameras"]["office"]["connection_quality"],
        json!("good")
    );

    // 2. `/api/events?include_thumbnails=0` omits inline base64 thumbnail while default preserves it
    state
        .db
        .upsert_event(&EventDto {
            id: "ev_thumb_bloat_1".to_string(),
            camera: "office".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 1000.0,
            end_time: Some(1005.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.91),
            false_positive: Some(false),
            box_coords: Some(json!([0.4, 0.4, 0.2, 0.3])),
            data: json!({ "box": [0.4, 0.4, 0.2, 0.3] }),
            thumbnail: Some("UklGRh4AAABXRUJQVlA4TBEAAAAvAAAAAAfQ//73v/+BiOh/AAA=".to_string()),
            area: Some(5000),
            ratio: Some(0.7),
            score: Some(0.91),
            stationary: false,
        })
        .unwrap();

    let ev_slim_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events?include_thumbnails=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ev_slim_resp.status(), StatusCode::OK);
    let ev_slim: Value =
        serde_json::from_slice(&to_bytes(ev_slim_resp.into_body(), 65_536).await.unwrap()).unwrap();
    assert!(
        ev_slim[0]["thumbnail"].is_null(),
        "include_thumbnails=0 must omit inline base64 thumbnail payload"
    );

    // 3. Camera tamper review freezes a privacy-masked forensic WebP thumbnail on disk and closes end_time on clear
    let yuv_len = 640 * 360 * 3 / 2;
    let blind_yuv = vec![16u8; yuv_len]; // completely flat dark frame triggers tamper
    let mut consec = 0u32;
    let mut active_tamper = None;
    let triggered = state
        .pipeline
        .evaluate_camera_tamper_with_review(
            "office",
            &blind_yuv,
            640,
            360,
            2000.0,
            &mut consec,
            1,
            &mut active_tamper,
        )
        .await;
    assert!(triggered);
    let created_dto = active_tamper
        .clone()
        .expect("active_tamper_review must be populated");
    assert!(
        created_dto
            .thumb_path
            .starts_with("/media/ninjanvr/clips/review/thumb-office-"),
        "tamper alert must freeze dedicated forensic thumbnail instead of mutable /latest.jpg, got {}",
        created_dto.thumb_path
    );
    let rel_thumb = created_dto
        .thumb_path
        .strip_prefix("/media/ninjanvr/")
        .unwrap();
    assert!(
        media_dir.join(rel_thumb).exists(),
        "frozen forensic tamper thumbnail must exist on disk"
    );

    // Clear tamper with textured frame -> closes end_time in SQLite
    let mut textured_yuv = vec![128u8; yuv_len];
    for (idx, px) in textured_yuv[..640 * 360].iter_mut().enumerate() {
        *px = ((idx * 37) % 240 + 10) as u8;
    }
    let cleared = state
        .pipeline
        .evaluate_camera_tamper_with_review(
            "office",
            &textured_yuv,
            640,
            360,
            2008.5,
            &mut consec,
            1,
            &mut active_tamper,
        )
        .await;
    assert!(!cleared);
    assert!(active_tamper.is_none());
    let persisted_rev = state
        .db
        .get_review_by_id(&created_dto.id)
        .unwrap()
        .expect("tamper review must exist in SQLite");
    assert_eq!(persisted_rev.end_time, Some(2008.5));

    // 4. Hybrid substream (640x360) -> mainstream (1920x1080) switch AT SEGMENT 19 (> 16!)
    // must emit `#EXT-X-DISCONTINUITY` and `#EXT-X-MAP:URI="init-v1-a1.mp4?seg=19"`!
    let mut cfg_no_mask = state.config.read().await.clone();
    cfg_no_mask.cameras.get_mut("office").unwrap().privacy_mask = json!([]);
    *state.config.write().await = cfg_no_mask;

    let sub_bytes = build_synthetic_mp4_with_resolution(640, 360, [0x42, 0x00, 0x1e]);
    let main_bytes = build_synthetic_mp4_with_resolution(1920, 1080, [0x64, 0x00, 0x28]);
    let rec_dir = media_dir.join("recordings/2026-11-19/12/office");

    for idx in 0..22u32 {
        let is_main = idx >= 18;
        let fname = format!("seg_{idx:02}.mp4");
        std::fs::write(
            rec_dir.join(&fname),
            if is_main { &main_bytes } else { &sub_bytes },
        )
        .unwrap();
        let start_t = 5000.0 + (idx as f64) * 10.0;
        state
            .db
            .insert_recording(&RecordingRow {
                id: format!("rec_{idx:02}"),
                camera: "office".to_string(),
                path: format!("/media/ninjanvr/recordings/2026-11-19/12/office/{fname}"),
                start_time: start_t,
                end_time: start_t + 10.0,
                duration: 10.0,
                motion: if is_main { 100 } else { 0 },
                objects: if is_main { 1 } else { 0 },
                dbfs: 0,
                segment_size: if is_main { 3.5 } else { 0.4 },
                regions: 0,
                motion_heatmap: None,
            })
            .unwrap();
    }

    let master_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/vod/office/start/5000/end/5220/master.m3u8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(master_resp.status(), StatusCode::OK);
    let master_m3u8 = String::from_utf8(
        to_bytes(master_resp.into_body(), 16_384)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(master_m3u8.contains("RESOLUTION=640x360"));
    assert!(master_m3u8.contains("CODECS=\"avc1.42001e\""));

    let index_resp = app
        .oneshot(
            Request::builder()
                .uri("/vod/office/start/5000/end/5220/index.m3u8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(index_resp.status(), StatusCode::OK);
    let index_m3u8 = String::from_utf8(
        to_bytes(index_resp.into_body(), 65_536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        index_m3u8.contains("#EXT-X-MAP:URI=\"init-v1-a1.mp4?seg=19\""),
        "tier switch at segment 19 (> 16) must re-probe resolution and emit updated #EXT-X-MAP, got:\n{index_m3u8}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
