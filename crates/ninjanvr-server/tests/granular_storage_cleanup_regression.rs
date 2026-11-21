use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use ninjanvr_db::{EventDto, ExportRow, RecordingRow, ReviewItemDto};
use ninjanvr_server::{JwtClaims, build_router_with_auth, new_app_state};
use serde_json::{Value, json};
use tower::ServiceExt;

fn now_sec() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn mint_cookie(jwt_secret: &str, sub: &str, role: &str, allowed_cameras: Option<&str>) -> String {
    let iat = now_sec() as u64;
    let claims = JwtClaims {
        sub: sub.to_string(),
        role: role.to_string(),
        allowed_cameras: allowed_cameras.map(str::to_string),
        exp: iat + 3600,
        iat,
    };
    let tok = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(jwt_secret.as_bytes()),
    )
    .unwrap();
    format!("ninjanvr_token={tok}")
}

#[tokio::test]
async fn granular_storage_breakdown_rbac_scoping_and_cleanup_targets_work_end_to_end() {
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr-granular-storage-{}",
        rand::random::<u64>()
    ));
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(media_dir.join("clips/review")).unwrap();
    std::fs::create_dir_all(media_dir.join("clips/previews/escritorio")).unwrap();
    std::fs::create_dir_all(media_dir.join("clips/dataset")).unwrap();
    std::fs::create_dir_all(media_dir.join("dataset")).unwrap();
    std::fs::create_dir_all(media_dir.join("models")).unwrap();
    std::fs::create_dir_all(media_dir.join("recordings/2026-11-10/10/escritorio")).unwrap();
    std::fs::create_dir_all(media_dir.join("recordings/2026-11-20/10/escritorio")).unwrap();
    std::fs::create_dir_all(media_dir.join("recordings/2026-11-20/10/cozinha")).unwrap();
    std::fs::create_dir_all(media_dir.join("exports")).unwrap();
    std::fs::write(web_dir.join("index.html"), "<html>Console</html>").unwrap();

    let db_path = tmp_dir.join("ninjanvr.db");
    let cfg_path = tmp_dir.join("config.yml");
    let yaml = format!(
        r#"
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
go2rtc:
  streams:
    escritorio:
      - "rtsp://cam:pass@192.0.2.10:554/stream1"
    cozinha:
      - "rtsp://cam:pass@192.0.2.11:554/stream1"
cameras:
  escritorio:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/escritorio
          roles: [detect, record]
    record:
      enabled: true
      continuous:
        days: 2
      alerts:
        pre_capture: 5
        post_capture: 5
        retain:
          days: 14
      detections:
        retain:
          days: 14
  cozinha:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/cozinha
          roles: [detect, record]
    record:
      enabled: true
      continuous:
        days: 3
      alerts:
        retain:
          days: 7
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();

    // Create users: unrestricted admin, camera-restricted operator (`cozinha`), and viewer
    state
        .db
        .create_user_with_cameras("root_admin", "AdminPass#1234", "admin", "*", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("cozinha_op", "OpPass#123456", "operator", "cozinha", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("viewer_user", "ViewerPass#1234", "viewer", "*", 1000)
        .unwrap();

    let now = now_sec();
    let four_days_ago = now - 4.0 * 86_400.0;
    let five_days_ago = now - 5.0 * 86_400.0;
    let six_days_ago = now - 6.0 * 86_400.0;
    let one_hour_ago = now - 3600.0;

    // 1. Populate recordings:
    // - escritorio recent continuous segment (within 2d)
    let p_esc_cont = media_dir.join("recordings/2026-11-20/10/escritorio/00.10.mp4");
    std::fs::write(&p_esc_cont, vec![0x11u8; 4096]).unwrap();
    state
        .db
        .insert_recording(&RecordingRow {
            id: "seg_esc_cont".to_string(),
            camera: "escritorio".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-20/10/escritorio/00.10.mp4".to_string(),
            start_time: one_hour_ago,
            end_time: one_hour_ago + 10.0,
            duration: 10.0,
            motion: 0,
            objects: 0,
            dbfs: -20,
            segment_size: 2.0,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();

    // - escritorio 4-day-old event-only segment (no reviewsegment -> must be pruned when events are cleared)
    let p_esc_evt_only = media_dir.join("recordings/2026-11-10/10/escritorio/00.15.mp4");
    std::fs::write(&p_esc_evt_only, vec![0x21u8; 4096]).unwrap();
    state
        .db
        .insert_recording(&RecordingRow {
            id: "seg_esc_evt_only".to_string(),
            camera: "escritorio".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-10/10/escritorio/00.15.mp4".to_string(),
            start_time: four_days_ago,
            end_time: four_days_ago + 10.0,
            duration: 10.0,
            motion: 90,
            objects: 1,
            dbfs: -18,
            segment_size: 4.0,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

    // - escritorio 5-day-old segment overlapping an active `reviewsegment` (must survive `target: "events"`!)
    let p_esc_rev_backed = media_dir.join("recordings/2026-11-10/10/escritorio/00.20.mp4");
    std::fs::write(&p_esc_rev_backed, vec![0x22u8; 8192]).unwrap();
    state
        .db
        .insert_recording(&RecordingRow {
            id: "seg_esc_old_evt".to_string(),
            camera: "escritorio".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-10/10/escritorio/00.20.mp4".to_string(),
            start_time: five_days_ago,
            end_time: five_days_ago + 10.0,
            duration: 10.0,
            motion: 120,
            objects: 2,
            dbfs: -18,
            segment_size: 10.0,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

    // - escritorio 6-day-old PINNED event segment (must survive `include_pinned: false`)
    let p_esc_pinned = media_dir.join("recordings/2026-11-10/10/escritorio/00.30.mp4");
    std::fs::write(&p_esc_pinned, vec![0x33u8; 4096]).unwrap();
    state
        .db
        .insert_recording(&RecordingRow {
            id: "seg_esc_pinned".to_string(),
            camera: "escritorio".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-10/10/escritorio/00.30.mp4".to_string(),
            start_time: six_days_ago,
            end_time: six_days_ago + 10.0,
            duration: 10.0,
            motion: 80,
            objects: 1,
            dbfs: -18,
            segment_size: 5.0,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

    // - cozinha motion segment
    let p_coz_mot = media_dir.join("recordings/2026-11-20/10/cozinha/00.10.mp4");
    std::fs::write(&p_coz_mot, vec![0x44u8; 2048]).unwrap();
    state
        .db
        .insert_recording(&RecordingRow {
            id: "seg_coz_mot".to_string(),
            camera: "cozinha".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-20/10/cozinha/00.10.mp4".to_string(),
            start_time: one_hour_ago,
            end_time: one_hour_ago + 10.0,
            duration: 10.0,
            motion: 50,
            objects: 0,
            dbfs: -25,
            segment_size: 3.0,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();

    // 2. Populate events & media artifacts (including custom `thumb_path` on pinned event)
    let ev_esc_old_webp = media_dir.join("clips/escritorio-ev_esc_old.webp");
    let ev_esc_pin_custom_webp = media_dir.join("clips/custom_pinned_snap.webp");
    let ev_coz_webp = media_dir.join("clips/cozinha-ev_coz_1.webp");
    let latest_esc_jpg = media_dir.join("clips/latest_escritorio.jpg");
    std::fs::write(&ev_esc_old_webp, vec![0xAAu8; 1500]).unwrap();
    std::fs::write(&ev_esc_pin_custom_webp, vec![0xBBu8; 1200]).unwrap();
    std::fs::write(&ev_coz_webp, vec![0xCCu8; 900]).unwrap();
    std::fs::write(&latest_esc_jpg, vec![0xDDu8; 600]).unwrap();

    for (id, cam, ts, pinned, thumb_path) in [
        (
            "ev_esc_old",
            "escritorio",
            four_days_ago,
            false,
            "/clips/escritorio-ev_esc_old.webp",
        ),
        (
            "ev_esc_pin",
            "escritorio",
            six_days_ago,
            true,
            "/clips/custom_pinned_snap.webp",
        ),
        (
            "ev_coz_1",
            "cozinha",
            one_hour_ago,
            false,
            "/clips/cozinha-ev_coz_1.webp",
        ),
    ] {
        state
            .db
            .upsert_event(&EventDto {
                id: id.to_string(),
                camera: cam.to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: ts,
                end_time: Some(ts + 8.0),
                has_clip: false,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: pinned,
                sub_label: None,
                top_score: Some(0.92),
                false_positive: Some(false),
                box_coords: None,
                data: json!({ "thumb_path": thumb_path }),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.92),
                stationary: false,
            })
            .unwrap();
    }

    // 3. Populate reviewsegment, previews, preview_frames, exports (completed + in-progress), dataset & models
    let rev_esc_webp = media_dir.join("clips/review/thumb-escritorio-rev_esc_1.webp");
    std::fs::write(&rev_esc_webp, vec![0xEEu8; 1100]).unwrap();
    state
        .db
        .insert_review_segment(&ReviewItemDto {
            id: "rev_esc_1".to_string(),
            camera: "escritorio".to_string(),
            start_time: five_days_ago,
            end_time: Some(five_days_ago + 12.0),
            severity: "alert".to_string(),
            thumb_path: "/clips/review/thumb-escritorio-rev_esc_1.webp".to_string(),
            has_been_reviewed: false,
            data: json!({ "objects": ["person"] }),
        })
        .unwrap();

    let prev_mp4 = media_dir.join("clips/previews/escritorio/1700000000-1700003600.mp4");
    std::fs::write(&prev_mp4, vec![0x55u8; 2000]).unwrap();
    state
        .db
        .insert_preview(
            "prev_esc_1",
            "escritorio",
            "/media/ninjanvr/clips/previews/escritorio/1700000000-1700003600.mp4",
            five_days_ago,
            five_days_ago + 3600.0,
            3600.0,
        )
        .unwrap();
    state
        .preview_frames
        .write_frame("escritorio", 1_700_000_500_000, &[0xFF, 0xD8, 0xFF, 0xD9])
        .unwrap();

    let exp_done_mp4 = media_dir.join("exports/exp_done.mp4");
    let exp_done_webp = media_dir.join("clips/exp_done.webp");
    let exp_active_mp4 = media_dir.join("exports/exp_active.mp4");
    let exp_active_webp = media_dir.join("clips/exp_active.webp");
    std::fs::write(&exp_done_mp4, vec![0x66u8; 3000]).unwrap();
    std::fs::write(&exp_done_webp, vec![0x77u8; 500]).unwrap();
    std::fs::write(&exp_active_mp4, vec![0x88u8; 3000]).unwrap();
    std::fs::write(&exp_active_webp, vec![0x89u8; 400]).unwrap();
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_done".to_string(),
            camera: "escritorio".to_string(),
            name: "Done Export".to_string(),
            date: five_days_ago,
            video_path: "/media/ninjanvr/exports/exp_done.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_done.webp".to_string(),
            in_progress: false,
        })
        .unwrap();
    state
        .db
        .insert_export(&ExportRow {
            id: "exp_active".to_string(),
            camera: "escritorio".to_string(),
            name: "Active Export".to_string(),
            date: now,
            video_path: "/media/ninjanvr/exports/exp_active.mp4".to_string(),
            thumb_path: "".to_string(),
            in_progress: true,
        })
        .unwrap();

    // Protected dataset & model files must never be touched by cleanup
    let protected_ds = media_dir.join("clips/dataset/ds_keep.webp");
    let protected_model = media_dir.join("models/yolo11.onnx");
    std::fs::write(&protected_ds, vec![0x99u8; 1024]).unwrap();
    std::fs::write(&protected_model, vec![0x9Au8; 2048]).unwrap();

    let app = build_router_with_auth(state.clone(), true);
    let admin_cookie = mint_cookie(&state.jwt_secret, "root_admin", "admin", None);
    let coz_op_cookie = mint_cookie(&state.jwt_secret, "cozinha_op", "operator", Some("cozinha"));
    let viewer_cookie = mint_cookie(&state.jwt_secret, "viewer_user", "viewer", None);

    // A. Viewer is rejected (403 Forbidden) on both breakdown and cleanup
    let viewer_get = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/storage/breakdown")
                .header(header::COOKIE, &viewer_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(viewer_get.status(), StatusCode::FORBIDDEN);

    // B. Cross-origin CSRF cookie request is rejected on POST /api/storage/cleanup
    let csrf_bad = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/storage/cleanup")
                .header(header::COOKIE, &admin_cookie)
                .header(header::HOST, "nvr.example.test")
                .header(header::ORIGIN, "https://evil.example.test")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"target":"events"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(csrf_bad.status(), StatusCode::FORBIDDEN);

    // C. Camera-restricted operator (`cozinha`) sees ONLY `cozinha` in breakdown and cannot execute admin-only cleanup
    let coz_bd_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/storage/breakdown")
                .header(header::COOKIE, &coz_op_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(coz_bd_res.status(), StatusCode::OK);
    let coz_bd: Value =
        serde_json::from_slice(&to_bytes(coz_bd_res.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(coz_bd["scoped"], json!(true));
    assert!(coz_bd["cameras"].get("cozinha").is_some());
    assert!(coz_bd["cameras"].get("escritorio").is_none());
    assert_eq!(coz_bd["categories"]["database"]["bytes"], json!(0));

    let coz_forbidden_cleanup = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/storage/cleanup")
                .header(header::COOKIE, &coz_op_cookie)
                .header(header::HOST, "localhost")
                .header(header::ORIGIN, "http://localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"target":"events","camera":"cozinha"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(coz_forbidden_cleanup.status(), StatusCode::FORBIDDEN);

    // D. Full admin GET /api/storage/breakdown & GET /api/stats verify granular metrics
    let stats_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/stats")
                .header(header::COOKIE, &admin_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stats_res.status(), StatusCode::OK);
    let stats_json: Value =
        serde_json::from_slice(&to_bytes(stats_res.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    let esc_stat = &stats_json["service"]["camera_storage"]["escritorio"];
    assert_eq!(esc_stat["usage"], json!(21.0));
    assert_eq!(esc_stat["continuous_mb"], json!(2.0));
    assert_eq!(esc_stat["event_segments_mb"], json!(19.0));
    assert_eq!(esc_stat["events_count"], json!(2));
    assert_eq!(esc_stat["pinned_events_count"], json!(1));
    assert_eq!(esc_stat["reviews_count"], json!(1));

    let bd_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/storage/breakdown")
                .header(header::COOKIE, &admin_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bd_res.status(), StatusCode::OK);
    let bd: Value =
        serde_json::from_slice(&to_bytes(bd_res.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert_eq!(bd["scoped"], json!(false));
    assert_eq!(bd["cameras"]["escritorio"]["events_count"], json!(2));
    assert_eq!(bd["cameras"]["escritorio"]["pinned_events_count"], json!(1));
    assert_eq!(
        bd["cameras"]["escritorio"]["retained_past_continuous_count"],
        json!(3)
    );
    assert_eq!(bd["categories"]["events_media"]["bytes"], json!(3600));
    assert_eq!(bd["categories"]["reviews_media"]["bytes"], json!(1100));
    assert_eq!(bd["categories"]["previews_media"]["bytes"], json!(2004));
    assert_eq!(bd["categories"]["exports_media"]["bytes"], json!(6900));
    assert!(bd["categories"]["database"]["bytes"].as_u64().unwrap() > 0);

    // E. Granular `events` cleanup on `escritorio` (keep pinned = true, prune_retained_segments = true)
    let clean_ev_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/storage/cleanup")
                .header(header::COOKIE, &admin_cookie)
                .header(header::HOST, "localhost")
                .header(header::ORIGIN, "http://localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"target":"events","camera":"escritorio","include_pinned":false,"prune_retained_segments":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(clean_ev_res.status(), StatusCode::OK);
    let clean_ev_json: Value = serde_json::from_slice(
        &to_bytes(clean_ev_res.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(clean_ev_json["deleted_items"], json!(1)); // ev_esc_old deleted, ev_esc_pin kept
    assert_eq!(clean_ev_json["deleted_segments"], json!(1)); // seg_esc_evt_only pruned; seg_esc_old_evt protected by rev_esc_1!
    assert!(!ev_esc_old_webp.exists());
    assert!(
        ev_esc_pin_custom_webp.exists(),
        "pinned event custom thumb_path must remain"
    );
    assert!(
        !p_esc_evt_only.exists(),
        "event-only excess MP4 must be pruned"
    );
    assert!(
        p_esc_rev_backed.exists(),
        "excess MP4 still backing an active reviewsegment must remain until review is cleaned"
    );
    assert!(p_esc_pinned.exists(), "pinned event MP4 must remain");
    assert!(p_esc_cont.exists(), "recent continuous MP4 must remain");
    assert!(
        exp_active_webp.exists(),
        "in-progress export thumbnail must remain"
    );
    assert!(
        ev_coz_webp.exists(),
        "other camera event must remain untouched"
    );
    assert!(
        latest_esc_jpg.exists(),
        "latest_*.jpg live frame must remain"
    );
    assert!(protected_ds.exists(), "dataset sample must remain");
    assert!(protected_model.exists(), "model weights must remain");

    // F. Granular `reviews`, `excess_recordings`, `previews`, `exports`, and `wal_checkpoint` cleanups
    for (payload, check_gone) in [
        (r#"{"target":"reviews"}"#, &rev_esc_webp),
        (
            r#"{"target":"excess_recordings","camera":"escritorio"}"#,
            &p_esc_rev_backed,
        ),
        (r#"{"target":"previews"}"#, &prev_mp4),
        (r#"{"target":"exports"}"#, &exp_done_mp4),
    ] {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/storage/cleanup")
                    .header(header::COOKIE, &admin_cookie)
                    .header(header::HOST, "localhost")
                    .header(header::ORIGIN, "http://localhost")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(
            !check_gone.exists(),
            "expected {:?} to be cleaned",
            check_gone
        );
    }
    assert!(
        !exp_done_webp.exists(),
        "completed export thumb must be cleaned"
    );
    assert!(
        exp_active_mp4.exists(),
        "in-progress export must never be deleted during exports cleanup"
    );
    assert!(
        !state
            .preview_frames
            .contains("escritorio", 1_700_000_500_000),
        "PreviewFrameStore in-memory index must be synchronized after previews cleanup"
    );

    let wal_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/storage/cleanup")
                .header(header::COOKIE, &admin_cookie)
                .header(header::HOST, "localhost")
                .header(header::ORIGIN, "http://localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"target":"wal_checkpoint"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wal_res.status(), StatusCode::OK);

    state.shutdown().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}
