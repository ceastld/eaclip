use super::*;
use std::cell::RefCell;

thread_local! {
    static BEFORE_IO: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}

pub(crate) fn before_receive_io() {
    let callback = BEFORE_IO.with(|slot| slot.borrow_mut().take());
    if let Some(callback) = callback {
        callback();
    }
}

fn fixture() -> (tempfile::TempDir, HostState, Value) {
    crate::clipboard::force_headless(true);
    let directory = tempfile::tempdir().unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(directory.path().join("host")),
        28965,
    )
    .unwrap();
    host.add_text(&json!({"text":"independent history item"}))
        .unwrap();
    let request = json!({
        "id":"receive", "senderDeviceId":"peer", "senderAlias":"test",
        "targetKind":"direct", "text":"", "createdAtUtc":Utc::now(),
        "attachments":[{"id":"file", "name":"receive.txt", "mimeType":"text/plain",
            "sizeBytes":5, "sha256":crate::lan::sha256_hex(b"hello")}]
    });
    (directory, host, request)
}

#[test]
fn lan_receive_io_keeps_search_and_window_commands_responsive() {
    let mut blocked = Vec::new();
    for operation in ["prepare", "upload", "cancel"] {
        let (_directory, host, request) = fixture();
        let web_session = host.create_session();
        if operation != "prepare" {
            host.lan_prepare_inbound("peer", &request).unwrap();
        }
        let (entered, entering) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let worker_host = host.clone();
        let worker = thread::spawn(move || {
            BEFORE_IO.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    entered.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(10)).unwrap();
                }))
            });
            match operation {
                "prepare" => worker_host
                    .lan_prepare_inbound("peer", &request)
                    .map(|_| ()),
                "upload" => worker_host.lan_upload_inbound("peer", "receive", "file", b"hello", 5),
                "cancel" => {
                    worker_host.lan_cancel_inbound("peer", "receive");
                    Ok(())
                }
                _ => unreachable!(),
            }
        });
        entering
            .recv_timeout(Duration::from_secs(5))
            .expect("must reach real receiver I/O path");
        let (done, completed) = mpsc::channel();
        let reader_host = host.clone();
        let reader = thread::spawn(move || {
            assert_eq!(reader_host.health()["service"], HEALTH_SERVICE_ID);
            let items = reader_host
                .search_items(
                    &json!({"webSessionId":web_session["webSessionId"],"query":"independent"}),
                )
                .unwrap();
            assert_eq!(items["total"], 1);
            reader_host.apply_window_action("setCollapsed", true);
            reader_host.apply_window_action("hide", true);
            assert!(!reader_host.inner.lock().window_visible);
            done.send(items).unwrap();
        });
        let responsive = completed.recv_timeout(Duration::from_millis(500)).is_ok();
        // Release and join even on failure so no test leaves a locked worker.
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
        reader.join().unwrap();
        if !responsive {
            blocked.push(operation);
        }
        if operation == "upload" {
            assert_eq!(
                host.lan_commit_inbound("peer", "receive").unwrap()["delivered"],
                true
            );
            let rows = host
                .lan_messages(&json!({"conversationId":"dm:peer"}))
                .unwrap();
            let file = rows[0]["attachments"][0]["localPath"].as_str().unwrap();
            assert_eq!(std::fs::read(file).unwrap(), b"hello");
        }
    }
    assert!(
        blocked.is_empty(),
        "receive I/O blocked host state: {blocked:?}"
    );
}
