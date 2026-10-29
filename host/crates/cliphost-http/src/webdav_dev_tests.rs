use super::*;
use tower::ServiceExt;

#[tokio::test]
async fn webdav_dev_routes_keep_secrets_out_of_status_and_reject_browser_origins() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .join(".tmp/webdav-http-tests");
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::tempdir_in(root).unwrap();
    let paths = cliphost_core::AppPaths::from_data_root(dir.path().join("data"));
    paths.ensure().unwrap();
    let host = if cliphost_core::is_headless() {
        HostState::open_isolated_visual(paths, 29065)
    } else {
        HostState::open_keyboard_input_qa(paths, 29065)
    }
    .unwrap();
    let app = router(HttpState {
        host,
        events: event_bus(),
    });
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/dev/webdav")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let status: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status["devOnly"], true);
    assert_eq!(status["configured"], false);
    assert!(status.get("recoveryKey").is_none());
    assert!(status.get("password").is_none());
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/dev/webdav")
                .header(header::ORIGIN, "https://untrusted.example")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response=app.oneshot(axum::http::Request::builder().method(Method::POST)
        .uri("/api/dev/webdav").header(header::CONTENT_TYPE,"application/json")
        .body(axum::body::Body::from(serde_json::json!({"command":"configure","endpoint":"http://127.0.0.1:1/private/","username":"synthetic-user","password":"synthetic-secret-never-echo"}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-secret-never-echo"));
}
