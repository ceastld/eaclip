use super::mobile_sync::*;
use crate::lan_tls::HttpReq;
use cliphost_core::{clipboard_sync::SyncEvent, paths::AppPaths, HostState};
use serde_json::{json, Value};
use std::time::Duration;

fn fixture() -> (
    tempfile::TempDir,
    HostState,
    std::sync::Arc<SyncHub>,
    String,
) {
    cliphost_core::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().to_path_buf()), 49765).unwrap();
    host.sync_set_enabled(true).unwrap();
    let invitation = host.sync_invitation().unwrap();
    let paired = host
        .sync_pair_client(&json!({"deviceId":"android-test","pairCode":invitation["pairCode"]}))
        .unwrap();
    let token = paired["token"].as_str().unwrap().to_string();
    let hub = SyncHub::new(host.clone());
    (dir, host, hub, token)
}
fn req(method: &str, path: &str, token: &str, body: Value) -> HttpReq {
    let body = if body.is_null() {
        Vec::new()
    } else {
        serde_json::to_vec(&body).unwrap()
    };
    HttpReq {
        device_sync_observation: None,
        method: method.into(),
        path: format!("{PREFIX}{path}"),
        query: String::new(),
        headers: vec![
            ("X-ClipLink-Device".into(), "android-test".into()),
            ("Authorization".into(), format!("Bearer {token}")),
        ],
        content_length: body.len() as i64,
        body,
        keep_alive: true,
        body_fully_consumed: true,
    }
}
fn response(response: SyncResponse) -> (u16, Value) {
    match response {
        SyncResponse::Json(code, body) => (code, body),
        _ => panic!("Expected JSON"),
    }
}

#[tokio::test]
async fn mobile_sync_authenticated_text_applies_once_and_rejects_spoofed_origin() {
    let (_dir, host, hub, token) = fixture();
    let event = SyncEvent::text(
        "evt_1".into(),
        "android-test".into(),
        1,
        " \r\n测试😀  ".into(),
    );
    let (code, ack) = response(
        hub.handle(req("POST", "/events", &token, json!(event)))
            .await,
    );
    assert_eq!(code, 200);
    assert_eq!(ack["status"], "applied");
    let (_, retry) = response(
        hub.handle(req("POST", "/events", &token, json!(event)))
            .await,
    );
    assert_eq!(retry, ack);
    let event = SyncEvent::text("evt_2".into(), host.sync_device_id(), 2, "spoof".into());
    assert_eq!(
        response(
            hub.handle(req("POST", "/events", &token, json!(event)))
                .await
        )
        .0,
        403
    );
    host.sync_forget_client("android-test").unwrap();
    assert_eq!(
        response(
            hub.handle(req(
                "POST",
                "/acks",
                &token,
                json!({"eventId":"evt_1","status":"applied"})
            ))
            .await
        )
        .0,
        401
    );
}

#[tokio::test]
async fn mobile_sync_offline_replay_is_accepted_without_native_application() {
    let (_dir, _host, hub, token) = fixture();
    let mut event = json!(SyncEvent::text(
        "offline_1".into(),
        "android-test".into(),
        1,
        "old text".into()
    ));
    event["replay"] = json!(true);
    let (code, ack) = response(hub.handle(req("POST", "/events", &token, event)).await);
    assert_eq!(code, 200);
    assert_eq!(ack["status"], "superseded");
}

#[tokio::test]
async fn mobile_sync_poll_wakes_on_event_and_marks_only_preconnection_events_replay() {
    let (_dir, _host, hub, _token) = fixture();
    hub.publish(SyncEvent::text(
        "old".into(),
        "desktop".into(),
        1,
        "old".into(),
    ));
    let old = hub.poll(0, Duration::from_secs(2), true).await;
    assert_eq!(old["events"][0]["replay"], true);
    let poll = {
        let hub = hub.clone();
        tokio::spawn(async move { hub.poll(1, Duration::from_secs(25), true).await })
    };
    tokio::task::yield_now().await;
    hub.publish(SyncEvent::text(
        "new".into(),
        "desktop".into(),
        2,
        "new".into(),
    ));
    let result = tokio::time::timeout(Duration::from_secs(1), poll)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["events"][0]["replay"], false);
    let reset = hub.poll(999, Duration::from_secs(25), false).await;
    assert_eq!(reset["reset"], true);
    assert_eq!(reset["cursor"], 2);
}
