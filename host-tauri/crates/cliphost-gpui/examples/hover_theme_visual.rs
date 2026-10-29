//! Debug-only, pure GPUI color fixture. No HostState, user data, capture,
//! API/RPC listener, clipboard hooks or input injection is initialized.

#[cfg(not(debug_assertions))]
fn main() {
    eprintln!("hover_theme_visual is available only in Debug builds.");
    std::process::exit(2);
}

#[cfg(debug_assertions)]
fn main() -> anyhow::Result<()> {
    fixture::run()
}

#[cfg(debug_assertions)]
mod fixture {
    use std::{
        fs,
        path::Path,
        time::{Duration, Instant},
    };

    use anyhow::{anyhow, bail, Context as _, Result};
    use cliphost_gpui::{
        motion, platform,
        preview::{DesignTokens, TOKENS_JSON},
    };
    use gpui::{
        div, prelude::*, px, size, App, Application, Bounds, Context, Hsla, IntoElement, Render,
        Rgba, SharedString, Timer, Window, WindowBounds, WindowOptions,
    };
    use serde_json::{json, Value};

    const WIDTH: f32 = 760.0;
    const HEIGHT: f32 = 660.0;
    const PROGRESS: [f32; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];

    fn color(hex: &str) -> Hsla {
        gpui::rgb(u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap()).into()
    }

    fn rgba_json(color: Hsla) -> Value {
        let c: Rgba = color.into();
        json!({"r":c.r, "g":c.g, "b":c.b, "a":c.a})
    }

    fn samples(tokens: &DesignTokens) -> Value {
        json!([false, true].map(|dark| {
            let t = tokens.theme(dark);
            let normal: Hsla = Rgba::default().into();
            let hover = color(&t.hover);
            let background: Rgba = color(&t.background).into();
            json!({
                "theme":if dark {"dark"} else {"light"},
                "background":t.background, "hover":t.hover,
                "samples":PROGRESS.map(|progress| {
                    let enter = motion::mix_hsla(normal, hover, progress);
                    let leave = motion::mix_hsla(hover, normal, progress);
                    json!({"progress":progress, "enter":rgba_json(enter),
                        "leave":rgba_json(leave),
                        "compositedEnter":rgba_json(background.blend(enter.into()).into()),
                        "compositedLeave":rgba_json(background.blend(leave.into()).into())})
                })
            })
        }))
    }

    struct HoverThemeVisual {
        tokens: DesignTokens,
    }

    impl Render for HoverThemeVisual {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .bg(gpui::rgb(0xffffff))
                .font_family(SharedString::from(
                    self.tokens.typography.font_family.clone(),
                ))
                .text_size(px(14.0))
                .children([true, false].map(|dark| {
                    let t = self.tokens.theme(dark);
                    let normal: Hsla = Rgba::default().into();
                    let hover = color(&t.hover);
                    div()
                        .w(px(WIDTH / 2.0))
                        .h_full()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .p(px(16.0))
                        .gap(px(8.0))
                        .bg(color(&t.background))
                        .text_color(color(&t.text))
                        .child(
                            div()
                                .text_size(px(20.0))
                                .child(if dark { "Dark" } else { "Light" }),
                        )
                        .child(
                            div()
                                .text_color(color(&t.text_muted))
                                .child(format!("背景 {} · hover {}", t.background, t.hover)),
                        )
                        .children([false, true].map(|leave| {
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .child(div().h(px(26.0)).flex().items_center().child(if leave {
                                    "Hover 离开"
                                } else {
                                    "Hover 进入"
                                }))
                                .children(PROGRESS.map(|progress| {
                                    let fill = if leave {
                                        motion::mix_hsla(hover, normal, progress)
                                    } else {
                                        motion::mix_hsla(normal, hover, progress)
                                    };
                                    div()
                                        .h(px(36.0))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .rounded(px(self.tokens.layout.row_radius))
                                        .px(px(self.tokens.layout.row_padding_x))
                                        .bg(fill)
                                        .child(format!("文本条目 · 进度 {:.0}%", progress * 100.0))
                                }))
                        }))
                        .child(
                            div()
                                .text_color(color(&t.text_muted))
                                .child("生产 mix_hsla · 无输入 / 无业务宿主"),
                        )
                }))
        }
    }

    fn diagnostics(window: &Window, tokens: &DesignTokens, started: Instant, done: bool) -> Value {
        #[cfg(windows)]
        let hwnd = platform::hwnd(window).ok().map(|value| value.0 as isize);
        #[cfg(not(windows))]
        let hwnd: Option<isize> = None;
        #[cfg(windows)]
        let is_foreground = hwnd.is_some_and(|hwnd| unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize == hwnd
        });
        #[cfg(not(windows))]
        let is_foreground = false;
        json!({
            "fixture":"hover-theme-visual", "pid":std::process::id(), "hwnd":hwnd,
            "width":WIDTH, "height":HEIGHT, "scaleFactor":window.scale_factor(),
            "elapsedMs":started.elapsed().as_millis(), "selfExitRequested":done,
            "debugOnly":true, "headless":true, "pureDrawing":true, "hostState":false,
            "nativeHost":false, "apiListener":false, "capture":false, "hooks":false,
            "rpc":false, "loginStartup":false, "userDataRead":false,
            "focusRequested":false, "inputInjected":false, "isForeground":is_foreground,
            "samples":samples(tokens), "nativeError":platform::take_error(window).ok().flatten()
        })
    }

    pub fn run() -> Result<()> {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()?;
        let evidence = repo.join(".tmp/panel-theme-hover");
        fs::create_dir_all(&evidence)?;
        let evidence = evidence.canonicalize()?;
        if !evidence.starts_with(&repo) {
            bail!("fixture evidence must stay within repository");
        }
        let report = evidence.join("window-report.json");
        if fs::symlink_metadata(&report).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!("report must be a regular local file");
        }
        let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
        let started = Instant::now();
        Application::new().run(move |cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx);
            let handle = cx
                .open_window(
                    WindowOptions {
                        titlebar: None,
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        focus: false,
                        show: false,
                        window_background: gpui::WindowBackgroundAppearance::Opaque,
                        ..Default::default()
                    },
                    |window, cx| {
                        window.set_window_title("ClipHost · isolated hover color fixture");
                        cx.new(|_| HoverThemeVisual { tokens })
                    },
                )
                .expect("create pure hover color fixture");
            handle
                .update(cx, |_, window, _| -> Result<(), String> {
                    platform::set_no_activate(window, true)?;
                    platform::set_geometry(
                        window,
                        None,
                        Some((f64::from(WIDTH), f64::from(HEIGHT))),
                    )?;
                    platform::show_no_activate(window)
                })
                .expect("configure fixture window")
                .expect("show without activation");
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.spawn(async move |cx| loop {
                Timer::after(Duration::from_millis(100)).await;
                let done = started.elapsed() >= Duration::from_secs(8);
                let Ok(value) = handle.update(cx, |view, window, _| {
                    diagnostics(window, &view.tokens, started, done)
                }) else {
                    break;
                };
                let result = serde_json::to_vec_pretty(&value)
                    .context("serialize diagnostics")
                    .and_then(|bytes| fs::write(&report, bytes).context("save diagnostics"));
                if let Err(error) = result {
                    eprintln!("{error:#}");
                }
                if done {
                    cx.update(|cx| cx.quit()).ok();
                    break;
                }
            })
            .detach();
        });
        Ok(())
    }
}
