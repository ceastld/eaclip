use cliphost_core::{quicker_drop::Bindings, HostState};
use std::{cell::{Cell, RefCell}, rc::Rc, sync::mpsc};
use tauri::Manager;

thread_local! { static BINDINGS: RefCell<Option<Bindings>> = const { RefCell::new(None) }; }
thread_local! { static STOPPING: Cell<bool> = const { Cell::new(false) }; }

fn report(host: &HostState, error: &str) {
    tracing::warn!(%error, "Panel action drop failed");
    host.show_host_toast(error, "error", Some("拖入动作失败"), Some(5000), None);
}

fn create(window: tauri::WebviewWindow, host: HostState) -> Result<Bindings, String> {
    let (sender, receiver) = mpsc::sync_channel::<(String, Option<String>)>(32);
    let worker_host = host.clone();
    std::thread::Builder::new()
        .name("panel-action-drop".into())
        .spawn(move || {
            while let Ok((payload, parent)) = receiver.recv() {
                if let Err(error) =
                    worker_host.panel_drop_quicker_action(&payload, parent.as_deref())
                {
                    report(&worker_host, &error);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Bindings::new(
        Rc::new(move |payload| {
            let result = payload.and_then(|payload| {
                let parent = host.panel_quicker_drop_parent();
                sender
                    .try_send((payload, parent))
                    .map_err(|_| "动作拖入队列暂不可用，请稍后重试。".to_string())
            });
            if let Err(error) = &result {
                report(&host, error);
            }
            result
        }),
        Rc::new(move |over| {
            let _ = window.eval(&format!(
                "document.documentElement.classList.toggle('panel-quicker-drag-over', {over});"
            ));
        }),
    )
    .map_err(|e| e.to_string())
}

pub fn refresh(window: &tauri::WebviewWindow, host: &HostState) {
    if STOPPING.with(Cell::get) {
        return;
    }
    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    BINDINGS.with(|cell| {
        // OLE can pump native messages; skip a reentrant refresh while attaching.
        let Ok(mut slot) = cell.try_borrow_mut() else {
            return;
        };
        if slot.is_none() {
            match create(window.clone(), host.clone()) {
                Ok(bindings) => *slot = Some(bindings),
                Err(error) => {
                    tracing::warn!(%error, "Panel drop setup failed");
                    return;
                }
            }
        }
        slot.as_mut().unwrap().refresh(hwnd.0 as isize);
    });
}

pub fn watch(window: &tauri::WebviewWindow, host: &HostState) {
    refresh(window, host);
    let app = window.app_handle().clone();
    let host = host.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let Some(window) = app.get_webview_window("panel") else {
                break;
            };
            let host = host.clone();
            if app
                .run_on_main_thread(move || refresh(&window, &host))
                .is_err()
            {
                break;
            }
        }
    });
}

pub fn clear() {
    BINDINGS.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() {
            *slot = None;
        }
    });
}

/// Revoke OLE targets on the UI STA while COM and WebView are still alive.
/// Process exit does not necessarily deliver a Destroyed event for the panel;
/// leaving these in TLS invokes RevokeDragDrop during DLL teardown and crashes.
pub fn shutdown() {
    // RevokeDragDrop can pump messages, including an already queued refresh.
    STOPPING.with(|stopping| stopping.set(true));
    clear();
}
