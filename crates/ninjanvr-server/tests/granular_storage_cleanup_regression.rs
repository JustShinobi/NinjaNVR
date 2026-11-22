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

    // 3. Populate reviewsegment (non-pinned + pinned-overlapping + cozinha), previews, preview_frames, exports, dataset & models
    let rev_esc_webp = media_dir.join("clips/review/thumb-escritorio-rev_esc_1.webp");
    let rev_esc_pin_webp = media_dir.join("clips/review/thumb-escritorio-rev_esc_pin.webp");
    let rev_coz_webp = media_dir.join("clips/review/thumb-cozinha-rev_coz_1.webp");
    std::fs::write(&rev_esc_webp, vec![0xEEu8; 1100]).unwrap();
    std::fs::write(&rev_esc_pin_webp, vec![0xEFu8; 700]).unwrap();
    std::fs::write(&rev_coz_webp, vec![0xF0u8; 500]).unwrap();
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
    state
        .db
        .insert_review_segment(&ReviewItemDto {
            id: "rev_esc_pin".to_string(),
            camera: "escritorio".to_string(),
            start_time: six_days_ago,
            end_time: Some(six_days_ago + 12.0),
            severity: "alert".to_string(),
            thumb_path: "/clips/review/thumb-escritorio-rev_esc_pin.webp".to_string(),
            has_been_reviewed: false,
            data: json!({ "objects": ["person"] }),
        })
        .unwrap();
    state
        .db
        .insert_review_segment(&ReviewItemDto {
            id: "rev_coz_1".to_string(),
            camera: "cozinha".to_string(),
            start_time: one_hour_ago,
            end_time: Some(one_hour_ago + 12.0),
            severity: "detection".to_string(),
            thumb_path: "/clips/review/thumb-cozinha-rev_coz_1.webp".to_string(),
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
    assert_eq!(esc_stat["reviews_count"], json!(2));

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
    assert_eq!(bd["categories"]["reviews_media"]["bytes"], json!(2300));
    assert_eq!(bd["categories"]["previews_media"]["bytes"], json!(2004));
    assert_eq!(bd["categories"]["exports_media"]["bytes"], json!(6900));
    assert!(bd["categories"]["database"]["bytes"].as_u64().unwrap() > 0);

    // E1. When `prune_retained_segments: false`, `events` cleanup on `escritorio` (> 7d = none, then > 3d)
    //     Wait — test `older_than_days: 7.0` with `prune_retained_segments: false` first as a no-op,
    //     then add a transient event `ev_esc_noprune` and verify `prune_retained_segments: false` preserves reviews & MP4s!
    let ev_esc_noprune_webp = media_dir.join("clips/escritorio-ev_esc_noprune.webp");
    std::fs::write(&ev_esc_noprune_webp, vec![0xABu8; 800]).unwrap();
    state
        .db
        .upsert_event(&EventDto {
            id: "ev_esc_noprune".to_string(),
            camera: "escritorio".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: five_days_ago,
            end_time: Some(five_days_ago + 5.0),
            has_clip: false,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.9),
            false_positive: Some(false),
            box_coords: None,
            data: json!({ "thumb_path": "/clips/escritorio-ev_esc_noprune.webp" }),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.9),
            stationary: false,
        })
        .unwrap();

    let clean_ev_no_prune = app
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
                    r#"{"target":"events","camera":"escritorio","older_than_days":4.5,"include_pinned":false,"prune_retained_segments":false}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(clean_ev_no_prune.status(), StatusCode::OK);
    let clean_ev_no_prune_json: Value = serde_json::from_slice(
        &to_bytes(clean_ev_no_prune.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(clean_ev_no_prune_json["deleted_items"], json!(1));
    assert_eq!(clean_ev_no_prune_json["deleted_segments"], json!(0));
    assert!(!ev_esc_noprune_webp.exists());
    assert!(
        rev_esc_webp.exists() && p_esc_rev_backed.exists(),
        "prune_retained_segments=false must preserve reviews and MP4 segments"
    );

    // E2. Granular `events` cleanup on `escritorio` (`include_pinned: false`, `prune_retained_segments: true`)
    //     Purges non-pinned events AND non-pinned reviews, freeing all excess MP4 segments (> 2d continuous)
    //     even if they had a review, while strictly preserving pinned (`★`) events, reviews overlapping pinned events, and pinned MP4s!
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
    assert_eq!(clean_ev_json["deleted_items"], json!(2)); // ev_esc_old + rev_esc_1 deleted; ev_esc_pin + rev_esc_pin kept!
    assert_eq!(clean_ev_json["deleted_segments"], json!(2)); // BOTH seg_esc_evt_only and seg_esc_old_evt freed!
    assert!(!ev_esc_old_webp.exists());
    assert!(
        !rev_esc_webp.exists(),
        "non-pinned review thumb must be removed when prune_retained_segments=true"
    );
    assert!(
        rev_esc_pin_webp.exists(),
        "review overlapping pinned event must remain protected"
    );
    assert!(
        ev_esc_pin_custom_webp.exists(),
        "pinned event custom thumb_path must remain"
    );
    assert!(
        !p_esc_evt_only.exists(),
        "event-only excess MP4 must be pruned"
    );
    assert!(
        !p_esc_rev_backed.exists(),
        "review-backed excess MP4 must ALSO be freed when prune_retained_segments=true"
    );
    assert!(p_esc_pinned.exists(), "pinned event MP4 must remain");
    assert!(p_esc_cont.exists(), "recent continuous MP4 must remain");
    assert!(
        exp_active_webp.exists(),
        "in-progress export thumbnail must remain"
    );
    assert!(
        ev_coz_webp.exists() && rev_coz_webp.exists(),
        "other camera event & review must remain untouched"
    );
    assert!(
        latest_esc_jpg.exists(),
        "latest_*.jpg live frame must remain"
    );
    assert!(protected_ds.exists(), "dataset sample must remain");
    assert!(protected_model.exists(), "model weights must remain");

    // F. Granular `reviews`, `excess_recordings`, `previews`, `exports`, and `wal_checkpoint` cleanups
    for (payload, check_gone) in [
        (r#"{"target":"reviews","camera":"cozinha"}"#, &rev_coz_webp),
        (
            r#"{"target":"excess_recordings","camera":"escritorio","include_pinned":true}"#,
            &p_esc_pinned,
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

#[tokio::test]
async fn batch_events_and_recordings_cleanup_uses_indexed_unlink_and_prunes_empty_dirs() {
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr-batch-storage-cleanup-{}",
        rand::random::<u64>()
    ));
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
    let old_hour_dir = media_dir.join("recordings/2026-11-01/08/sala");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(media_dir.join("clips/review")).unwrap();
    std::fs::create_dir_all(media_dir.join("exports")).unwrap();
    std::fs::create_dir_all(&old_hour_dir).unwrap();
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
    sala:
      - "rtsp://cam:pass@192.0.2.20:554/stream1"
cameras:
  sala:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/sala
          roles: [detect, record]
    record:
      enabled: true
      continuous:
        days: 3
      alerts:
        retain:
          days: 14
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("root_admin", "AdminPass#1234", "admin", "*", 1000)
        .unwrap();

    let now = now_sec();
    let five_days_ago = now - 5.0 * 86_400.0;

    // Seed 16 events + 14 reviews + 16 MP4 segments (> 8 threshold to exercise single-pass directory indexing)
    for i in 0..16 {
        let seg_name = format!("00.{i:02}.mp4");
        let seg_disk = old_hour_dir.join(&seg_name);
        std::fs::write(&seg_disk, vec![0x31u8; 2048]).unwrap();
        state
            .db
            .insert_recording(&RecordingRow {
                id: format!("seg_sala_{i}"),
                camera: "sala".to_string(),
                path: format!("/media/ninjanvr/recordings/2026-11-01/08/sala/{seg_name}"),
                start_time: five_days_ago + (i as f64) * 10.0,
                end_time: five_days_ago + (i as f64) * 10.0 + 10.0,
                duration: 10.0,
                motion: 100,
                objects: 1,
                dbfs: -20,
                segment_size: 2.0,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();

        let snap_disk = media_dir.join(format!("clips/sala-ev_sala_{i}.webp"));
        std::fs::write(&snap_disk, vec![0x41u8; 512]).unwrap();
        state
            .db
            .upsert_event(&EventDto {
                id: format!("ev_sala_{i}"),
                camera: "sala".to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: five_days_ago + (i as f64) * 10.0,
                end_time: Some(five_days_ago + (i as f64) * 10.0 + 8.0),
                has_clip: false,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.95),
                false_positive: Some(false),
                box_coords: None,
                data: json!({ "thumb_path": format!("/clips/sala-ev_sala_{i}.webp") }),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.95),
                stationary: false,
            })
            .unwrap();
    }

    for i in 0..14 {
        let rev_disk = media_dir.join(format!("clips/review/thumb-sala-rev_sala_{i}.webp"));
        std::fs::write(&rev_disk, vec![0x51u8; 384]).unwrap();
        state
            .db
            .insert_review_segment(&ReviewItemDto {
                id: format!("rev_sala_{i}"),
                camera: "sala".to_string(),
                start_time: five_days_ago + (i as f64) * 10.0,
                end_time: Some(five_days_ago + (i as f64) * 10.0 + 10.0),
                severity: "alert".to_string(),
                thumb_path: format!("/clips/review/thumb-sala-rev_sala_{i}.webp"),
                has_been_reviewed: false,
                data: json!({ "objects": ["person"] }),
            })
            .unwrap();
    }

    let app = build_router_with_auth(state.clone(), true);
    let admin_cookie = mint_cookie(&state.jwt_secret, "root_admin", "admin", None);

    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/storage/cleanup")
                .header(header::COOKIE, &admin_cookie)
                .header(header::HOST, "localhost")
                .header(header::ORIGIN, "http://localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"target":"events","camera":"sala","include_pinned":false,"prune_retained_segments":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(res.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert_eq!(body["deleted_items"], json!(30)); // 16 events + 14 reviews
    assert_eq!(body["deleted_segments"], json!(16));
    assert!(
        !old_hour_dir.exists() && !media_dir.join("recordings/2026-11-01").exists(),
        "empty <camera>, HH, and YYYY-MM-DD directories must be pruned bottom-up"
    );
    assert!(
        media_dir.join("recordings").exists(),
        "recordings root directory must remain intact"
    );

    state.shutdown().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn surviving_pinned_review_span_review_prefixed_camera_and_cold_root_unlink_regression() {
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr-sec-storage-cleanup-{}",
        rand::random::<u64>()
    ));
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
    let cold_real = tmp_dir.join("cold_real");
    let cold_via_parent = tmp_dir.join("cold_parent_link/cold_tier");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(media_dir.join("clips/review")).unwrap();
    std::fs::create_dir_all(media_dir.join("exports")).unwrap();
    std::fs::create_dir_all(media_dir.join("recordings/2026-11-01/10/review-room")).unwrap();
    std::fs::create_dir_all(cold_real.join("cold_tier")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&cold_real, tmp_dir.join("cold_parent_link")).unwrap();
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
    review-room:
      - "rtsp://cam:pass@192.0.2.30:554/stream1"
    room:
      - "rtsp://cam:pass@192.0.2.31:554/stream1"
cameras:
  review-room:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/review-room
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
  room:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/room
          roles: [detect, record]
    record:
      enabled: true
      continuous:
        days: 2
      alerts:
        retain:
          days: 14
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("root_admin", "AdminPass#1234", "admin", "*", 1000)
        .unwrap();

    let now = now_sec();
    let six_days_ago = now - 6.0 * 86_400.0;

    // 1. Multi-segment pinned review span (`[t - 30s, t + 12s]`) wider than `ev_pin ± 5s` (`[t, t + 8s]`)
    let seg_pin_lead = media_dir.join("recordings/2026-11-01/10/review-room/00.00.mp4");
    let seg_pin_core = media_dir.join("recordings/2026-11-01/10/review-room/00.30.mp4");
    std::fs::write(&seg_pin_lead, vec![0x61u8; 2048]).unwrap();
    std::fs::write(&seg_pin_core, vec![0x62u8; 2048]).unwrap();
    state
        .db
        .insert_recording(&RecordingRow {
            id: "seg_rr_pin_lead".to_string(),
            camera: "review-room".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-01/10/review-room/00.00.mp4".to_string(),
            start_time: six_days_ago - 30.0,
            end_time: six_days_ago - 20.0,
            duration: 10.0,
            motion: 80,
            objects: 0,
            dbfs: -20,
            segment_size: 2.0,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
    state
        .db
        .insert_recording(&RecordingRow {
            id: "seg_rr_pin_core".to_string(),
            camera: "review-room".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-01/10/review-room/00.30.mp4".to_string(),
            start_time: six_days_ago,
            end_time: six_days_ago + 10.0,
            duration: 10.0,
            motion: 100,
            objects: 1,
            dbfs: -18,
            segment_size: 2.0,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

    // Active/pinned event on `review-room` with `-clean.webp` and `.mp4` clip in `clips/`
    // (backdated mtime > 60s so orphan sweep inspects them!)
    let rr_clean_webp = media_dir.join("clips/review-room-ev_rr_pin-clean.webp");
    let rr_clip_mp4 = media_dir.join("clips/review-room-ev_rr_pin.mp4");
    let rr_orphan_webp = media_dir.join("clips/review-room-ev_rr_gone.webp");
    std::fs::write(&rr_clean_webp, vec![0x71u8; 700]).unwrap();
    std::fs::write(&rr_clip_mp4, vec![0x72u8; 1400]).unwrap();
    std::fs::write(&rr_orphan_webp, vec![0x73u8; 500]).unwrap();
    let old_ftime = std::fs::FileTimes::new()
        .set_modified(UNIX_EPOCH + std::time::Duration::from_secs((six_days_ago as u64).max(1)));
    for p in [&rr_clean_webp, &rr_clip_mp4, &rr_orphan_webp] {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(p) {
            let _ = f.set_times(old_ftime);
        }
    }

    state
        .db
        .upsert_event(&EventDto {
            id: "ev_rr_pin".to_string(),
            camera: "review-room".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: six_days_ago,
            end_time: Some(six_days_ago + 8.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: true,
            sub_label: None,
            top_score: Some(0.96),
            false_positive: Some(false),
            box_coords: None,
            data: json!({}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.96),
            stationary: false,
        })
        .unwrap();
    state
        .db
        .insert_review_segment(&ReviewItemDto {
            id: "rev_rr_pin_wide".to_string(),
            camera: "review-room".to_string(),
            start_time: six_days_ago - 30.0,
            end_time: Some(six_days_ago + 12.0),
            severity: "alert".to_string(),
            thumb_path: "".to_string(),
            has_been_reviewed: false,
            data: json!({ "objects": ["person"] }),
        })
        .unwrap();

    let app = build_router_with_auth(state.clone(), true);
    let admin_cookie = mint_cookie(&state.jwt_secret, "root_admin", "admin", None);

    // A. Scoped cleanup on `camera: "room"` MUST NOT touch `review-room`'s orphan `review-room-ev_rr_gone.webp`
    let res_room = app
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
                    r#"{"target":"events","camera":"room","include_pinned":false,"prune_retained_segments":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_room.status(), StatusCode::OK);
    assert!(
        rr_orphan_webp.exists(),
        "cleaning camera 'room' must never unlink 'review-room-*' orphan files"
    );

    // B. Cleanup on `camera: "review-room"` (`include_pinned: false, prune_retained_segments: true`)
    //    must preserve BOTH `seg_rr_pin_lead` (covered by surviving `rev_rr_pin_wide`) and `ev_rr_pin`'s `-clean.webp`/`.mp4`,
    //    and refuse to cross-delete ambiguous `review-room-*` vs `review-<room>-*` orphans until global unscoped cleanup runs!
    let res_rr = app
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
                    r#"{"target":"events","camera":"review-room","include_pinned":false,"prune_retained_segments":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_rr.status(), StatusCode::OK);
    assert!(
        seg_pin_lead.exists() && seg_pin_core.exists(),
        "all MP4 segments backing surviving pinned-overlapping review must remain intact"
    );
    assert!(
        rr_clean_webp.exists() && rr_clip_mp4.exists(),
        "review-room active/pinned event clips & -clean.webp must never be stripped to 'room-*' and unlinked"
    );

    // B2. Unscoped admin cleanup (`camera: null`) safely unlinks genuine orphan `rr_orphan_webp`
    //     while still preserving `rr_clean_webp` and `rr_clip_mp4` for pinned `ev_rr_pin`!
    let res_global = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/storage/cleanup")
                .header(header::COOKIE, &admin_cookie)
                .header(header::HOST, "localhost")
                .header(header::ORIGIN, "http://localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"target":"events","include_pinned":false,"prune_retained_segments":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res_global.status(), StatusCode::OK);
    assert!(
        !rr_orphan_webp.exists(),
        "genuine unreferenced orphan file must be cleaned on unscoped admin cleanup"
    );
    assert!(
        rr_clean_webp.exists() && rr_clip_mp4.exists(),
        "pinned 'review-room' event clips & -clean.webp must survive unscoped admin cleanup too"
    );

    // C. Flat cold-tier MP4 under a non-canonical parent symlink (`cold_parent_link/cold_tier/flat_seg.mp4`) unlinks cleanly
    #[cfg(unix)]
    {
        let flat_cold_seg = cold_real.join("cold_tier/flat_seg.mp4");
        std::fs::write(&flat_cold_seg, vec![0x81u8; 1024]).unwrap();
        let (cnt, freed) = ninjanvr_server::media_lifecycle::unlink_deleted_recording_rows(
            &media_dir,
            Some(&cold_via_parent),
            "room",
            &[(
                "seg_flat_cold".to_string(),
                "/media/ninjanvr/recordings/flat_seg.mp4".to_string(),
                1.0,
            )],
        );
        assert_eq!(cnt, 1);
        assert_eq!(freed, 1024);
        assert!(
            !flat_cold_seg.exists(),
            "flat cold-tier segment under non-canonical cold_dir path must be unlinked from disk"
        );
    }

    state.shutdown().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn async_cleanup_job_streams_ws_progress_completes_and_enforces_camera_scope() {
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr-async-cleanup-ws-{}",
        rand::random::<u64>()
    ));
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
    let seg_dir = media_dir.join("recordings/2026-11-02/09/escritorio");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(media_dir.join("clips/review")).unwrap();
    std::fs::create_dir_all(media_dir.join("exports")).unwrap();
    std::fs::create_dir_all(&seg_dir).unwrap();
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
        days: 2
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("root_admin", "AdminPass#1234", "admin", "*", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("coz_op", "OpPass#123456", "operator", "cozinha", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("viewer_user", "ViewerPass#1234", "viewer", "*", 1000)
        .unwrap();

    let now = now_sec();
    let five_days_ago = now - 5.0 * 86_400.0;
    let seven_days_ago = now - 7.0 * 86_400.0;

    // Seed 520 non-pinned events, 260 non-pinned reviews, and 420 excess MP4 segments (multiple micro-batches),
    // plus 1 pinned event + 1 pinned-overlapping review + 1 pinned MP4 segment that must survive!
    for i in 0..520 {
        let snap = media_dir.join(format!("clips/escritorio-ev_batch_{i}.webp"));
        std::fs::write(&snap, vec![0x42u8; 256]).unwrap();
        state
            .db
            .upsert_event(&EventDto {
                id: format!("ev_batch_{i}"),
                camera: "escritorio".to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: five_days_ago + (i as f64),
                end_time: Some(five_days_ago + (i as f64) + 5.0),
                has_clip: false,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.91),
                false_positive: Some(false),
                box_coords: None,
                data: json!({ "thumb_path": format!("/clips/escritorio-ev_batch_{i}.webp") }),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.91),
                stationary: false,
            })
            .unwrap();
    }
    for i in 0..260 {
        let rthumb = media_dir.join(format!("clips/review/thumb-escritorio-rev_batch_{i}.webp"));
        std::fs::write(&rthumb, vec![0x43u8; 256]).unwrap();
        state
            .db
            .insert_review_segment(&ReviewItemDto {
                id: format!("rev_batch_{i}"),
                camera: "escritorio".to_string(),
                start_time: five_days_ago + (i as f64) * 2.0,
                end_time: Some(five_days_ago + (i as f64) * 2.0 + 10.0),
                severity: "alert".to_string(),
                thumb_path: format!("/clips/review/thumb-escritorio-rev_batch_{i}.webp"),
                has_been_reviewed: false,
                data: json!({ "objects": ["person"] }),
            })
            .unwrap();
    }
    for i in 0..420 {
        let sname = format!("seg_{i:04}.mp4");
        std::fs::write(seg_dir.join(&sname), vec![0x44u8; 1024]).unwrap();
        state
            .db
            .insert_recording(&RecordingRow {
                id: format!("rec_batch_{i}"),
                camera: "escritorio".to_string(),
                path: format!("/media/ninjanvr/recordings/2026-11-02/09/escritorio/{sname}"),
                start_time: five_days_ago + (i as f64),
                end_time: five_days_ago + (i as f64) + 1.0,
                duration: 1.0,
                motion: 80,
                objects: 1,
                dbfs: -20,
                segment_size: 1.0,
                regions: 1,
                motion_heatmap: None,
            })
            .unwrap();
    }

    // Pinned event + overlapping review + MP4 at `seven_days_ago`
    let pin_seg = seg_dir.join("pinned_keep.mp4");
    let pin_snap = media_dir.join("clips/escritorio-ev_keep_pin.webp");
    std::fs::write(&pin_seg, vec![0x77u8; 2048]).unwrap();
    std::fs::write(&pin_snap, vec![0x78u8; 512]).unwrap();
    state
        .db
        .insert_recording(&RecordingRow {
            id: "rec_keep_pin".to_string(),
            camera: "escritorio".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-02/09/escritorio/pinned_keep.mp4".to_string(),
            start_time: seven_days_ago,
            end_time: seven_days_ago + 10.0,
            duration: 10.0,
            motion: 90,
            objects: 1,
            dbfs: -18,
            segment_size: 2.0,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
    state
        .db
        .upsert_event(&EventDto {
            id: "ev_keep_pin".to_string(),
            camera: "escritorio".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: seven_days_ago,
            end_time: Some(seven_days_ago + 8.0),
            has_clip: false,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: true,
            sub_label: None,
            top_score: Some(0.99),
            false_positive: Some(false),
            box_coords: None,
            data: json!({ "thumb_path": "/clips/escritorio-ev_keep_pin.webp" }),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.99),
            stationary: false,
        })
        .unwrap();
    state
        .db
        .insert_review_segment(&ReviewItemDto {
            id: "rev_keep_pin".to_string(),
            camera: "escritorio".to_string(),
            start_time: seven_days_ago,
            end_time: Some(seven_days_ago + 10.0),
            severity: "alert".to_string(),
            thumb_path: "".to_string(),
            has_been_reviewed: false,
            data: json!({ "objects": ["person"] }),
        })
        .unwrap();

    let app = build_router_with_auth(state.clone(), true);
    let admin_cookie = mint_cookie(&state.jwt_secret, "root_admin", "admin", None);
    let coz_op_cookie = mint_cookie(&state.jwt_secret, "coz_op", "operator", Some("cozinha"));
    let viewer_cookie = mint_cookie(&state.jwt_secret, "viewer_user", "viewer", None);

    // RBAC + CSRF checks on `/api/jobs/cleanup`
    let viewer_list = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/jobs/cleanup")
                .header(header::COOKIE, &viewer_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(viewer_list.status(), StatusCode::FORBIDDEN);

    let coz_op_start = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/jobs/cleanup")
                .header(header::COOKIE, &coz_op_cookie)
                .header(header::HOST, "localhost")
                .header(header::ORIGIN, "http://localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"target":"events","camera":"cozinha","include_pinned":false,"prune_retained_segments":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(coz_op_start.status(), StatusCode::FORBIDDEN);

    let csrf_bad_start = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/jobs/cleanup")
                .header(header::COOKIE, &admin_cookie)
                .header(header::HOST, "nvr.example.test")
                .header(header::ORIGIN, "https://evil.example.test")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"target":"events"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(csrf_bad_start.status(), StatusCode::FORBIDDEN);

    let mut ws_rx = state.comms.subscribe_ws();

    // Start async job as `root_admin` -> returns `202 Accepted` immediately without blocking
    let start_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/jobs/cleanup")
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
    assert_eq!(start_res.status(), StatusCode::ACCEPTED);
    let initial_snap: Value =
        serde_json::from_slice(&to_bytes(start_res.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    let job_id = initial_snap["id"].as_str().unwrap().to_string();
    assert_eq!(initial_snap["target"], json!("events"));
    assert_eq!(initial_snap["camera"], json!("escritorio"));
    assert_eq!(initial_snap["cancellable"], json!(true));

    // Collect `/ws` `"storage_cleanup"` frames until terminal `"completed"`
    let mut snapshots: Vec<Value> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        let Ok(Ok(raw)) =
            tokio::time::timeout(std::time::Duration::from_secs(3), ws_rx.recv()).await
        else {
            break;
        };
        let Ok(envelope) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if envelope["topic"] == json!("storage_cleanup") {
            let payload_val: Value = match &envelope["payload"] {
                Value::String(s) => serde_json::from_str(s).unwrap(),
                v => v.clone(),
            };
            if payload_val["id"] == json!(job_id) {
                let is_done = payload_val["status"] == json!("completed")
                    || payload_val["status"] == json!("failed")
                    || payload_val["status"] == json!("cancelled");
                snapshots.push(payload_val);
                if is_done {
                    break;
                }
            }
        }
    }

    assert!(
        snapshots.len() >= 3,
        "expected multiple incremental /ws progress frames before completion, got {}",
        snapshots.len()
    );
    for pair in snapshots.windows(2) {
        let prev_pct = pair[0]["progress_pct"].as_f64().unwrap();
        let next_pct = pair[1]["progress_pct"].as_f64().unwrap();
        assert!(
            next_pct + 1e-6 >= prev_pct,
            "progress_pct must be monotonically non-decreasing ({prev_pct} -> {next_pct})"
        );
    }
    for snap in &snapshots {
        if snap["phase"] == json!("purging_catalog") {
            let pct = snap["progress_pct"].as_f64().unwrap();
            assert!(
                pct <= 45.0,
                "catalog stage must stay <= 45% when MP4 segment pruning follows (got {pct}%)"
            );
        }
    }
    let final_snap = snapshots.last().unwrap();
    assert_eq!(final_snap["status"], json!("completed"));
    assert_eq!(final_snap["progress_pct"], json!(100.0));
    assert_eq!(final_snap["deleted_items"], json!(780)); // 520 events + 260 reviews
    assert_eq!(final_snap["deleted_segments"], json!(420));
    assert!(final_snap["freed_bytes"].as_u64().unwrap() >= 420 * 1024);
    assert!(
        pin_seg.exists() && pin_snap.exists(),
        "pinned event snapshot and MP4 segment must survive async micro-batched cleanup"
    );

    // Defense-in-depth CameraScope isolation on `StorageCleanupJobManager` + operator 403 on `GET /api/jobs/cleanup`
    let coz_scope = ninjanvr_db::CameraScope::from_option(Some(vec!["cozinha".to_string()]));
    assert!(
        state
            .cleanup_jobs
            .list_for_scope(&coz_scope, now_sec())
            .is_empty(),
        "restricted CameraScope must not see jobs belonging to another camera"
    );
    let esc_scope = ninjanvr_db::CameraScope::from_option(Some(vec!["escritorio".to_string()]));
    assert_eq!(
        state
            .cleanup_jobs
            .list_for_scope(&esc_scope, now_sec())
            .len(),
        1
    );

    let coz_op_list_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/jobs/cleanup")
                .header(header::COOKIE, &coz_op_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(coz_op_list_res.status(), StatusCode::FORBIDDEN);

    let admin_list_res = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/jobs/cleanup")
                .header(header::COOKIE, &admin_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(admin_list_res.status(), StatusCode::OK);
    let admin_list: Value = serde_json::from_slice(
        &to_bytes(admin_list_res.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(admin_list.as_array().unwrap().len(), 1);
    assert_eq!(admin_list[0]["id"], json!(job_id));
    assert_eq!(admin_list[0]["status"], json!("completed"));

    state.shutdown().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn async_cleanup_job_conflict_and_cooperative_mid_run_cancellation_leave_zero_orphan_drift() {
    let tmp_dir = std::env::temp_dir().join(format!(
        "ninjanvr-async-cleanup-cancel-{}",
        rand::random::<u64>()
    ));
    let web_dir = tmp_dir.join("web");
    let media_dir = tmp_dir.join("media");
    let seg_dir = media_dir.join("recordings/2026-11-01/07/escritorio");
    std::fs::create_dir_all(&web_dir).unwrap();
    std::fs::create_dir_all(media_dir.join("clips/review")).unwrap();
    std::fs::create_dir_all(media_dir.join("exports")).unwrap();
    std::fs::create_dir_all(&seg_dir).unwrap();
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
  cozinha:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/cozinha
          roles: [detect, record]
    record:
      enabled: true
      continuous:
        days: 2
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let state = new_app_state(&cfg_path, web_dir, media_dir.clone(), false)
        .await
        .unwrap();
    state
        .db
        .create_user_with_cameras("root_admin", "AdminPass#1234", "admin", "*", 1000)
        .unwrap();
    state
        .db
        .create_user_with_cameras("coz_op", "OpPass#123456", "operator", "cozinha", 1000)
        .unwrap();

    let now = now_sec();
    let ten_days_ago = now - 10.0 * 86_400.0;

    // Seed 2,400 excess MP4 segments (12 micro-batches of 200 segments) so cooperative cancel catches it mid-flight
    let total_seeded = 2_400usize;
    for i in 0..total_seeded {
        let sname = format!("s_{i:04}.mp4");
        std::fs::write(seg_dir.join(&sname), vec![0x55u8; 512]).unwrap();
        state
            .db
            .insert_recording(&RecordingRow {
                id: format!("seg_cancel_{i}"),
                camera: "escritorio".to_string(),
                path: format!("/media/ninjanvr/recordings/2026-11-01/07/escritorio/{sname}"),
                start_time: ten_days_ago + (i as f64),
                end_time: ten_days_ago + (i as f64) + 1.0,
                duration: 1.0,
                motion: 20,
                objects: 0,
                dbfs: -25,
                segment_size: 0.5,
                regions: 0,
                motion_heatmap: None,
            })
            .unwrap();
    }

    let app = build_router_with_auth(state.clone(), true);
    let admin_cookie = mint_cookie(&state.jwt_secret, "root_admin", "admin", None);
    let coz_op_cookie = mint_cookie(&state.jwt_secret, "coz_op", "operator", Some("cozinha"));

    let mut ws_rx = state.comms.subscribe_ws();

    let start_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/jobs/cleanup")
                .header(header::COOKIE, &admin_cookie)
                .header(header::HOST, "localhost")
                .header(header::ORIGIN, "http://localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"target":"excess_recordings","camera":"escritorio"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(start_res.status(), StatusCode::ACCEPTED);
    let snap0: Value =
        serde_json::from_slice(&to_bytes(start_res.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    let job_id = snap0["id"].as_str().unwrap().to_string();

    // Operator (`coz_op`) cannot cancel admin cleanup job (`403 Forbidden`), and defense-in-depth
    // `CameraScope` on `request_cancel` returns `Forbidden` for out-of-scope cameras
    let coz_scope = ninjanvr_db::CameraScope::from_option(Some(vec!["cozinha".to_string()]));
    assert!(matches!(
        state
            .cleanup_jobs
            .request_cancel(&job_id, &coz_scope, now_sec()),
        ninjanvr_server::storage_ops::CancelCleanupJobOutcome::Forbidden
    ));

    let op_cancel = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/jobs/cleanup/{job_id}/cancel"))
                .header(header::COOKIE, &coz_op_cookie)
                .header(header::HOST, "localhost")
                .header(header::ORIGIN, "http://localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(op_cancel.status(), StatusCode::FORBIDDEN);

    let admin_conflict = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/jobs/cleanup")
                .header(header::COOKIE, &admin_cookie)
                .header(header::HOST, "localhost")
                .header(header::ORIGIN, "http://localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"target":"events","camera":"cozinha"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    if admin_conflict.status() == StatusCode::CONFLICT {
        let cbody: Value = serde_json::from_slice(
            &to_bytes(admin_conflict.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(cbody["job"]["id"], json!(job_id));
    }

    // Legacy synchronous `POST /api/storage/cleanup` also respects global single-job mutual exclusion (`409 Conflict`)
    let legacy_sync_conflict = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/storage/cleanup")
                .header(header::COOKIE, &admin_cookie)
                .header(header::HOST, "localhost")
                .header(header::ORIGIN, "http://localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"target":"events","camera":"cozinha"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    if state
        .cleanup_jobs
        .active_snapshots()
        .iter()
        .any(|s| s.id == job_id)
    {
        assert_eq!(legacy_sync_conflict.status(), StatusCode::CONFLICT);
    }

    // Request cooperative cancellation as `root_admin`
    let cancel_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/jobs/cleanup/{job_id}/cancel"))
                .header(header::COOKIE, &admin_cookie)
                .header(header::HOST, "localhost")
                .header(header::ORIGIN, "http://localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cancel_res.status(), StatusCode::OK);

    // Wait for terminal `/ws` frame for `job_id`
    let mut final_snap: Option<Value> = None;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        let Ok(Ok(raw)) =
            tokio::time::timeout(std::time::Duration::from_secs(3), ws_rx.recv()).await
        else {
            break;
        };
        let Ok(envelope) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if envelope["topic"] == json!("storage_cleanup") {
            let payload_val: Value = match &envelope["payload"] {
                Value::String(s) => serde_json::from_str(s).unwrap(),
                v => v.clone(),
            };
            if payload_val["id"] == json!(job_id)
                && (payload_val["status"] == json!("cancelled")
                    || payload_val["status"] == json!("completed"))
            {
                final_snap = Some(payload_val);
                break;
            }
        }
    }

    let term = final_snap.expect("expected terminal /ws frame after cancellation");
    assert_eq!(term["cancellable"], json!(false));
    let deleted_segs = term["deleted_segments"].as_u64().unwrap() as usize;
    if term["status"] == json!("cancelled") {
        assert!(
            deleted_segs < total_seeded,
            "cooperative cancellation should stop before deleting all {total_seeded} segments (deleted={deleted_segs})"
        );
    }

    // Invariant: ZERO DB-vs-disk orphan drift after mid-run cancellation!
    let surviving_db = state
        .db
        .select_recordings_before_cutoff(
            "escritorio",
            now + 60.0,
            0.0,
            0.0,
            false,
            false,
            false,
            None,
        )
        .unwrap();
    assert_eq!(
        surviving_db.len() + deleted_segs,
        total_seeded,
        "surviving DB rows ({}) + deleted segments ({deleted_segs}) must equal seeded ({total_seeded})",
        surviving_db.len()
    );
    let on_disk_count = if seg_dir.exists() {
        std::fs::read_dir(&seg_dir).unwrap().flatten().count()
    } else {
        0
    };
    assert_eq!(
        on_disk_count,
        surviving_db.len(),
        "every surviving DB row must have its MP4 on disk and every deleted DB row must have its MP4 unlinked"
    );

    state.shutdown().await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}
