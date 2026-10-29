//! Isolated renderer scheduling check: no HostState, database or clipboard.
use gpui::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Probe(Arc<AtomicUsize>);
impl Render for Probe {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.0.fetch_add(1, Ordering::SeqCst);
        // Same continuation API as GIF/WebP and style animations.
        window.request_animation_frame();
        div().size_full().bg(rgb(0x202020))
    }
}

fn next(window: &Window, count: Arc<AtomicUsize>) {
    window.on_next_frame(move |window, _| {
        if count.fetch_add(1, Ordering::SeqCst) < 2 {
            next(window, count);
        }
    });
}

#[cfg(windows)]
fn show(window: &Window, visible: bool) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::*};
    let RawWindowHandle::Win32(handle) = HasWindowHandle::window_handle(window).unwrap().as_raw()
    else {
        panic!("expected own Windows HWND")
    };
    unsafe {
        let _ = ShowWindow(
            HWND(handle.hwnd.get() as *mut _),
            if visible { SW_SHOWNOACTIVATE } else { SW_HIDE },
        );
    }
}

fn main() {
    let output = std::env::args()
        .nth(1)
        .expect("receipt path under repo .tmp required");
    let output = std::path::PathBuf::from(output);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap()
        .join(".tmp");
    let parent = output.parent().unwrap().canonicalize().unwrap();
    assert!(parent.starts_with(root), "receipt must be under repo .tmp");
    assert!(!output.exists(), "receipt must be new");
    let renders = Arc::new(AtomicUsize::new(0));
    let callbacks = Arc::new(AtomicUsize::new(0));
    Application::new().run(move |cx| {
        let count = renders.clone();
        let window = cx
            .open_window(
                WindowOptions {
                    show: false,
                    focus: false,
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        point(px(0.), px(0.)),
                        size(px(48.), px(48.)),
                    ))),
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Probe(count)),
            )
            .unwrap();
        cx.spawn(async move |cx| {
            Timer::after(Duration::from_millis(250)).await;
            let initial = renders.load(Ordering::SeqCst);
            assert!(initial > 0, "initial hidden frame never rendered");
            Timer::after(Duration::from_millis(250)).await;
            assert_eq!(
                renders.load(Ordering::SeqCst),
                initial,
                "idle hidden window kept redrawing"
            );
            let started = std::time::Instant::now();
            window
                .update(cx, |_, window, _| next(window, callbacks.clone()))
                .unwrap();
            for _ in 0..100 {
                if callbacks.load(Ordering::SeqCst) == 3 {
                    break;
                }
                Timer::after(Duration::from_millis(10)).await;
            }
            assert_eq!(
                callbacks.load(Ordering::SeqCst),
                3,
                "hidden next-frame callbacks stalled"
            );
            let callback_ms = started.elapsed().as_millis();
            let started = std::time::Instant::now();
            window.update(cx, |_, _, cx| cx.notify()).unwrap();
            for _ in 0..100 {
                if renders.load(Ordering::SeqCst) > initial {
                    break;
                }
                Timer::after(Duration::from_millis(10)).await;
            }
            assert!(
                renders.load(Ordering::SeqCst) > initial,
                "dirty hidden view was not woken"
            );
            let refresh_ms = started.elapsed().as_millis();
            Timer::after(Duration::from_millis(100)).await;
            let settled = renders.load(Ordering::SeqCst);
            Timer::after(Duration::from_millis(250)).await;
            assert_eq!(
                renders.load(Ordering::SeqCst),
                settled,
                "redraw did not return to idle"
            );
            #[cfg(windows)]
            {
                window
                    .update(cx, |_, window, _| show(window, true))
                    .unwrap();
                Timer::after(Duration::from_millis(150)).await;
                assert!(
                    renders.load(Ordering::SeqCst) > settled + 1,
                    "visible animation did not resume"
                );
                window
                    .update(cx, |_, window, _| show(window, false))
                    .unwrap();
                Timer::after(Duration::from_millis(100)).await;
                let hidden_again = renders.load(Ordering::SeqCst);
                Timer::after(Duration::from_millis(250)).await;
                assert_eq!(
                    renders.load(Ordering::SeqCst),
                    hidden_again,
                    "animation kept redrawing after Hide"
                );
            }
            std::fs::write(
                output,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "status":"passed", "hidden":true, "clipboardAccessed":false,
                    "databaseOpened":false, "initialRenders":initial,
                    "nextFrameCallbacks":3, "callbackChainMs":callback_ms,
                    "dirtyWakeMs":refresh_ms, "settledRenders":settled,
                    "visibleAnimationResumedAndHiddenAnimationPaused":cfg!(windows),
                }))
                .unwrap(),
            )
            .unwrap();
            cx.update(|cx| cx.quit()).unwrap();
        })
        .detach();
    });
}
