//! Opt-in native acceptance only. Fixture metadata is added while the clipboard
//! is still locked, so other running clipboard managers cannot capture test data.
//! This module and its publication hook are absent from application builds.
use super::ClipboardMemory;
use crate::{
    clipboard, input_target::InputTarget, AppPaths, AppStore, ClipKind, ClipRecord, HostState,
};
use serde_json::{json, Value};
use std::{cell::RefCell, path::Path};
use windows::Win32::{
    Foundation::HWND,
    UI::{
        Input::KeyboardAndMouse::GetAsyncKeyState,
        WindowsAndMessaging::{GetWindowTextW, GetWindowThreadProcessId},
    },
};

thread_local! {
    static FIXTURE_NONCE: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(super) fn exclude_fixture_capture(
    formats: Vec<(u32, ClipboardMemory)>,
) -> Result<Vec<(u32, ClipboardMemory)>, String> {
    let nonce = FIXTURE_NONCE.with(|slot| slot.borrow().clone());
    let Some(nonce) = nonce else {
        return Ok(formats);
    };
    super::exclude_capture(formats, &nonce)
}

#[test]
#[ignore = "native clipboard with an owned in-memory backup supervisor required"]
fn cancelled_text_write_preserves_newer_clipboard() {
    let path = std::env::var("CLIPHOST_NATIVE_SELECTION_VERSION_FIXTURE")
        .expect("owned fixture request required");
    let path = Path::new(&path).canonicalize().unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(5)
        .unwrap();
    assert!(path.starts_with(workspace.join(".tmp").canonicalize().unwrap()));
    let request: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let nonce = request["nonce"].as_str().unwrap();
    assert!(nonce.len() == 32 && nonce.chars().all(|c| c.is_ascii_hexdigit()));
    let initial = request["clipboardSequence"].as_u64().unwrap() as u32;
    std::env::set_var("CLIPHOST_TAURI_HEADLESS", "0");
    clipboard::force_headless(false);
    FIXTURE_NONCE.with(|slot| *slot.borrow_mut() = Some(nonce.to_owned()));
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            FIXTURE_NONCE.with(|slot| *slot.borrow_mut() = None);
            clipboard::force_headless(true);
        }
    }
    let _clear = Clear;
    super::prepare_text("original selection")
        .unwrap()
        .write_if_current(Some(initial))
        .unwrap();
    let selected = clipboard::sequence_number();
    super::prepare_text("newer clipboard content")
        .unwrap()
        .write_if_current(Some(selected))
        .unwrap();
    let newer = clipboard::sequence_number();
    assert_ne!(selected, newer);
    let error = super::prepare_text("stale transformed selection")
        .unwrap()
        .write_if_current(Some(selected))
        .unwrap_err();
    assert!(error.contains("剪贴板已变化"));
    assert_eq!(
        clipboard::sequence_number(),
        newer,
        "a rejected write emptied the clipboard"
    );
    assert_eq!(
        clipboard::read_selection_text().as_deref(),
        Some("newer clipboard content")
    );
    assert!(clipboard::peek_snapshot().unwrap().exclude_monitor);
    std::fs::write(path.parent().unwrap().join("receipt.json"), serde_json::to_vec_pretty(&json!({
        "ok":true, "selectedSequence":selected, "newerSequence":newer, "finalSequence":clipboard::sequence_number(),
        "staleWriteRejected":true, "newerTextPreserved":true, "excludedFromCapture":true,
    })).unwrap()).unwrap();
}

#[test]
#[ignore = "explicit owned Chrome receiver, native clipboard/input, and in-memory clipboard backup required"]
fn paste_into_browser() {
    let request_path = std::env::var("CLIPHOST_NATIVE_BROWSER_FIXTURE")
        .expect("run only through the owned native browser workflow");
    let request_path = Path::new(&request_path).canonicalize().unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(5)
        .unwrap();
    assert!(request_path.starts_with(workspace.join(".tmp").canonicalize().unwrap()));
    let request: Value = serde_json::from_slice(&std::fs::read(&request_path).unwrap()).unwrap();
    let root = request_path.parent().unwrap();
    let nonce = request["nonce"].as_str().unwrap();
    assert!(nonce.len() >= 16 && nonce.chars().all(|c| c.is_ascii_hexdigit()));
    let raw_hwnd = request["hwnd"].as_i64().unwrap() as isize;
    let hwnd = HWND(raw_hwnd as *mut _);
    let mut pid = 0;
    let mut title = [0u16; 256];
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let len = GetWindowTextW(hwnd, &mut title).max(0) as usize;
        assert_eq!(pid, request["pid"].as_u64().unwrap() as u32);
        assert!(String::from_utf16_lossy(&title[..len]).starts_with("ClipHost Chrome Receiver"));
        for key in [1, 2, 16, 17, 18, 91, 92] {
            assert!(GetAsyncKeyState(key) >= 0, "user input held");
        }
    }
    clipboard::force_headless(true);
    let paths = AppPaths::from_data_root(root.join("data"));
    let kind = request["kind"].as_str().unwrap();
    let mut seeded = None;
    if matches!(kind, "html" | "image" | "files") {
        let mut item = ClipRecord::new_text("before edit", "native-fixture");
        item.kind = match kind {
            "html" => ClipKind::HtmlClipItem,
            "image" => ClipKind::ImageClipItem,
            _ => ClipKind::FileClipItem,
        };
        if kind == "html" {
            item.html_data = Some("<p>before edit</p>".into());
        }
        if matches!(kind, "image" | "files") {
            let value = request["path"].as_str().unwrap();
            assert!(Path::new(value).is_absolute());
            let path = Path::new(value).canonicalize().unwrap();
            assert!(path.starts_with(root));
            if kind == "image" {
                item.image_path = Some(value.to_owned());
            } else {
                item.file_paths = vec![value.to_owned()];
            }
            item.body_text = None;
        }
        let store = AppStore::open(&paths.db).unwrap();
        store.insert_clip(&item, true).unwrap();
        seeded = Some(item.id);
    }
    let host = HostState::open(paths, 28965).unwrap();
    let id = seeded.unwrap_or_else(|| {
        host.add_text(&json!({"text":"before edit"})).unwrap()["createdIds"][0]
            .as_str()
            .unwrap()
            .to_owned()
    });
    if matches!(kind, "text" | "favorite" | "html") {
        let revision = host.detail(&json!({"id":id})).unwrap()["revision"].clone();
        let saved = host
            .patch_item(&json!({"id":id, "expectedRevision":revision,
            "bodyText":request["text"], "htmlData":request["html"]}))
            .unwrap();
        assert_eq!(saved["bodyText"], request["text"]);
        let session = host.create_session();
        let found = host
            .search_items(&json!({"webSessionId":session["sessionId"], "query":"ChromeFixture"}))
            .unwrap();
        assert!(found["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == id));
    }
    let mut favor_id = None;
    if kind == "favorite" {
        host.favorite_from_clip(&json!({"id":id})).unwrap();
        let tree = host.favor_tree().unwrap();
        assert_eq!(tree["rootItems"].as_array().unwrap().len(), 1);
        favor_id = Some(tree["rootItems"][0].as_str().unwrap().to_owned());
    }
    std::env::set_var("CLIPHOST_TAURI_HEADLESS", "0");
    clipboard::force_headless(false);
    let target = InputTarget::capture(raw_hwnd).unwrap();
    target.verify().unwrap();
    host.set_input_runner(move |job| {
        target.verify()?;
        job()
    });
    FIXTURE_NONCE.with(|slot| *slot.borrow_mut() = Some(nonce.to_owned()));
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            FIXTURE_NONCE.with(|slot| *slot.borrow_mut() = None);
        }
    }
    let _clear = Clear;
    let result = if let Some(favor) = &favor_id {
        host.favor_paste(&json!({"id":favor}))
    } else {
        host.paste_item(&json!({"id":id}))
    };
    let receipt = json!({"ok":result.is_ok(), "result":result, "kind":kind, "clipId":id,
        "favorId":favor_id, "clipboardSequence":clipboard::sequence_number(),
        "excludedFromCapture":clipboard::peek_snapshot().is_some_and(|s| s.exclude_monitor),
        "detail":host.detail(&json!({"id":id})).unwrap()});
    std::fs::write(
        root.join("receipt.json"),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["ok"], true, "{receipt}");
    assert_eq!(receipt["excludedFromCapture"], true);
}
