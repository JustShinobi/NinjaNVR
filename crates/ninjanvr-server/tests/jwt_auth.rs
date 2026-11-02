use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ninjanvr_server::{build_router_with_auth, new_app_state};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn jwt_login_and_authenticated_cookie_roundtrip() {
    let tmp_dir = std::env::temp_dir().join(format!("ninjanvr-jwt-test-{}", rand::random::<u32>()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let db_path = tmp_dir.join("frigate.db");
    let cfg_path = tmp_dir.join("config.yml");
    let web_dir = tmp_dir.join("web-v2");
    std::fs::create_dir_all(&web_dir).unwrap();

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
      width: 640
      height: 480
      fps: 5
version: 0.18-0
"#,
        db_path.display()
    );
    std::fs::write(&cfg_path, yaml).unwrap();

    let pre_db = ninjanvr_db::NinjaDatabase::open(&db_path).unwrap();
    let admin_pw = pre_db
        .ensure_default_admin(1000)
        .unwrap()
        .expect("fresh db must create default admin");
    drop(pre_db);

    let state = new_app_state(&cfg_path, web_dir, tmp_dir.clone(), false)
        .await
        .unwrap();

    let app = build_router_with_auth(state, true);

    // 1. Unauthenticated request to protected route must return 401
    let unauth_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth_resp.status(), StatusCode::UNAUTHORIZED);

    // 2. POST /api/login encodes a signed HS256 JWT via jsonwebtoken::encode
    let login_payload = serde_json::to_vec(&json!({
        "user": "admin",
        "password": admin_pw
    }))
    .unwrap();
    let login_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(login_payload))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login_resp.status(), StatusCode::OK);

    let set_cookie = login_resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .expect("POST /api/login must return a Set-Cookie header");
    let cookie_pair = set_cookie
        .split(';')
        .next()
        .expect("cookie key=value must be present")
        .to_string();
    assert!(cookie_pair.starts_with("frigate_token="));

    // 3. Authenticated request decodes & verifies the HS256 JWT via jsonwebtoken::decode
    let auth_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth")
                .header(header::COOKIE, &cookie_pair)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(auth_resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        auth_resp
            .headers()
            .get("remote-user")
            .and_then(|v| v.to_str().ok()),
        Some("admin")
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
