use super::*;
use std::cell::RefCell;
use std::fs;

thread_local! {
    static BEFORE_SAVE: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}

pub(crate) fn before_save() {
    if let Some(callback) = BEFORE_SAVE.with(|slot| slot.borrow_mut().take()) {
        callback();
    }
}

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("host")), 28965).unwrap();
    (dir, host)
}

fn pair_request(host: &HostState, alias: &str) -> Value {
    json!({"deviceId":"persist-peer", "alias":alias, "baseUrl":"https://127.0.0.1:29766",
        "certificateFingerprint":"b".repeat(64), "tokenForReceiver":crate::lan::create_token(),
        "pairingCode":host.lan_refresh_code()["code"]})
}

#[test]
fn lan_save_io_keeps_search_and_window_commands_responsive() {
    let mut blocked = Vec::new();
    for operation in ["settings", "pair", "forget"] {
        let (_dir, host) = fixture();
        host.add_text(&json!({"text":"independent history"})).unwrap();
        let session = host.create_session();
        let request = pair_request(&host, "original");
        if operation == "forget" { host.lan_accept_inbound_pair(&request).unwrap(); }
        let (entered, entering) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let writer_host = host.clone();
        let writer = thread::spawn(move || {
            BEFORE_SAVE.with(|slot| *slot.borrow_mut() = Some(Box::new(move || {
                entered.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(10)).unwrap();
            })));
            match operation {
                "settings" => writer_host.lan_config_update(&json!({"deviceAlias":"saved"})),
                "pair" => writer_host.lan_accept_inbound_pair(&request),
                "forget" => writer_host.lan_forget(&json!({"deviceId":"persist-peer"})),
                _ => unreachable!(),
            }
        });
        entering.recv_timeout(Duration::from_secs(5)).expect("enter actual LAN save");
        let (done, completed) = mpsc::channel();
        let reader_host = host.clone();
        let reader = thread::spawn(move || {
            assert_eq!(reader_host.health()["service"], HEALTH_SERVICE_ID);
            let result = reader_host.search_items(&json!({
                "webSessionId":session["webSessionId"], "query":"independent"
            })).unwrap();
            assert_eq!(result["total"], 1);
            reader_host.apply_window_action("setCollapsed", true);
            reader_host.apply_window_action("hide", true);
            assert!(!reader_host.inner.lock().window_visible);
            done.send(()).unwrap();
        });
        let responsive = completed.recv_timeout(Duration::from_millis(500)).is_ok();
        release.send(()).unwrap();
        writer.join().unwrap().unwrap();
        reader.join().unwrap();
        if !responsive { blocked.push(operation); }
    }
    assert!(blocked.is_empty(), "LAN save blocked host state: {blocked:?}");
}

#[cfg(windows)]
#[test]
fn lan_settings_failed_save_preserves_state_and_retries() {
    use std::os::windows::fs::OpenOptionsExt;
    let (_dir, host) = fixture();
    let paths = host.inner.lock().paths.clone();
    host.lan_config_update(&json!({"deviceAlias":"original"})).unwrap();
    let path = paths.configs.join("LanShare/settings.json");
    let before = fs::read(&path).unwrap();
    let locked = fs::OpenOptions::new().read(true).share_mode(1).open(&path).unwrap();
    let request = json!({"deviceAlias":"changed", "enabled":true, "maxParallelDeliveries":8});
    assert!(host.lan_config_update(&request).is_err(), "failed disk write must not report success");
    assert_eq!(host.lan_status()["deviceAlias"], "original");
    assert_eq!(host.lan_status()["enabled"], false);
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(locked);
    host.lan_config_update(&request).unwrap();
    drop(host);
    let reopened = HostState::open(paths, 28965).unwrap();
    assert_eq!(reopened.lan_status()["deviceAlias"], "changed");
    assert_eq!(reopened.lan_status()["enabled"], true);
    assert_eq!(reopened.lan_status()["maxParallelDeliveries"], 8);
}

#[cfg(windows)]
#[test]
fn lan_pair_and_forget_failed_save_preserve_trust_and_retry() {
    use std::os::windows::fs::OpenOptionsExt;
    let mut incorrectly_succeeded = Vec::new();
    for operation in ["pair", "forget"] {
        let (_dir, host) = fixture();
        let original = host.lan_accept_inbound_pair(&pair_request(&host, "original")).unwrap();
        let auth = format!("Bearer {}", original["tokenForRequester"].as_str().unwrap());
        let paths = host.inner.lock().paths.clone();
        let path = paths.configs.join("LanShare/peers.json");
        let before = fs::read(&path).unwrap();
        let locked = fs::OpenOptions::new().read(true).share_mode(1).open(&path).unwrap();
        let request = pair_request(&host, "replacement");
        let result = if operation == "pair" {
            host.lan_accept_inbound_pair(&request)
        } else {
            host.lan_forget(&json!({"deviceId":"persist-peer"}))
        };
        if result.is_ok() {
            incorrectly_succeeded.push(operation);
            continue;
        }
        assert_eq!(host.lan_peers()[0]["alias"], "original");
        assert!(host.lan_authenticate(Some("persist-peer"), Some(&auth)).is_ok());
        assert_eq!(fs::read(&path).unwrap(), before);
        drop(locked);
        if operation == "pair" {
            let result = host.lan_accept_inbound_pair(&pair_request(&host, "replacement")).unwrap();
            let fresh = format!("Bearer {}", result["tokenForRequester"].as_str().unwrap());
            assert!(host.lan_authenticate(Some("persist-peer"), Some(&fresh)).is_ok());
            assert!(host.lan_authenticate(Some("persist-peer"), Some(&auth)).is_err());
        } else {
            assert_eq!(host.lan_forget(&json!({"deviceId":"persist-peer"})).unwrap()["ok"], true);
            assert!(host.lan_authenticate(Some("persist-peer"), Some(&auth)).is_err());
        }
        drop(host);
        let reopened = HostState::open(paths, 28965).unwrap();
        if operation == "pair" {
            assert_eq!(reopened.lan_peers()[0]["alias"], "replacement");
        } else {
            assert_eq!(reopened.lan_peers().as_array().unwrap().len(), 0);
        }
    }
    assert!(incorrectly_succeeded.is_empty(), "failed peer saves reported success: {incorrectly_succeeded:?}");
}

#[test]
fn lan_late_presence_keeps_forgotten_or_repaired_trust() {
    for operation in ["forget", "pair", "unchanged"] {
        let (_dir, host) = fixture();
        host.lan_accept_inbound_pair(&pair_request(&host, "original")).unwrap();
        let previous = host.lan.lock().discovered["persist-peer"].clone();
        let mut live = previous.clone();
        live.alias = "probe result".into();
        // The request started with `previous`; these actions happen before its response.
        match operation {
            "forget" => { host.lan_forget(&json!({"deviceId":"persist-peer"})).unwrap(); }
            "pair" => { host.lan_accept_inbound_pair(&pair_request(&host, "replacement")).unwrap(); }
            _ => {}
        }
        let file = host.lan.lock().peers_path.clone();
        let before = fs::read(&file).unwrap();
        let updated = host.lan.lock().refresh_presence(vec![(previous, live)]).unwrap();
        assert_eq!(updated, operation == "unchanged");
        match operation {
            "forget" => assert!(host.lan_peers().as_array().unwrap().is_empty()),
            "pair" => assert_eq!(host.lan_peers()[0]["alias"], "replacement"),
            _ => assert_eq!(host.lan_peers()[0]["alias"], "probe result"),
        }
        if operation != "unchanged" { assert_eq!(fs::read(&file).unwrap(), before); }
    }
}
