//! Isolated real row hovers: fresh headless core, generated files, no desktop input.
use super::{Category, Column, SessionKey, SuperMenuWindow};
use crate::{
    hover::HoverService,
    platform,
    preview::{DesignTokens, TOKENS_JSON},
    super_menu::style_fixture::{capture, output_directory},
    DesignAssets,
};
use anyhow::{anyhow, ensure, Context as _};
use cliphost_core::{AppPaths, HostState};
use gpui::{
    point, prelude::*, px, size, AnyWindowHandle, Application, Bounds, MouseMoveEvent,
    PlatformInput, Timer, WindowBounds, WindowHandle, WindowOptions,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use windows::{
    core::{w, BOOL},
    Win32::{
        Foundation::{HWND, LPARAM, RECT},
        Graphics::Gdi::GetWindowRgnBox,
        System::Threading::GetCurrentThreadId,
        UI::WindowsAndMessaging::*,
    },
};

fn owned_visible(owner: isize) -> Vec<isize> {
    struct Search {
        owner: isize,
        windows: Vec<isize>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, parameter: LPARAM) -> BOOL {
        let search = &mut *(parameter.0 as *mut Search);
        if IsWindowVisible(hwnd).as_bool()
            && GetWindow(hwnd, GW_OWNER).is_ok_and(|parent| parent.0 as isize == search.owner)
        {
            search.windows.push(hwnd.0 as isize);
        }
        BOOL(1)
    }
    let mut search = Search {
        owner,
        windows: Vec::new(),
    };
    unsafe {
        let _ = EnumThreadWindows(
            GetCurrentThreadId(),
            Some(visit),
            LPARAM(&mut search as *mut Search as isize),
        );
        let native = GetPropW(HWND(owner as *mut _), w!("ClipHost.NativeHoverPreview")).0 as isize;
        if native != 0
            && IsWindowVisible(HWND(native as *mut _)).as_bool()
            && GetWindow(HWND(native as *mut _), GW_OWNER)
                .is_ok_and(|parent| parent.0 as isize == owner)
            && !search.windows.contains(&native)
        {
            search.windows.push(native);
        }
    }
    search.windows
}

fn move_row(
    handle: WindowHandle<SuperMenuWindow>,
    row: usize,
    cx: &mut impl gpui::AppContext,
) -> anyhow::Result<u64> {
    AnyWindowHandle::from(handle).update(cx, |root, window, cx| {
        window.refresh();
        window.draw(cx).clear();
        let menu = root
            .downcast::<SuperMenuWindow>()
            .map_err(|_| anyhow!("Fixture root changed"))?;
        let bounds = menu
            .read(cx)
            .row_bounds
            .borrow()
            .get(&(0, row))
            .copied()
            .context("Menu row was not painted")?;
        window.dispatch_input(
            PlatformInput::MouseMove(MouseMoveEvent {
                position: point(bounds.left() + px(40.), bounds.center().y),
                ..Default::default()
            }),
            cx,
        );
        window.refresh();
        window.draw(cx).clear();
        let ticket = menu
            .read(cx)
            .file_preview_ticket
            .as_ref()
            .map(|(_, ticket)| ticket.id());
        ticket.context("Real row hover did not schedule a preview")
    })?
}

fn screenshot(
    owner: isize,
    beside: [i32; 4],
    path: &std::path::Path,
    report: &Arc<Mutex<Value>>,
    name: &str,
) -> anyhow::Result<Value> {
    let windows = owned_visible(owner);
    ensure!(
        windows.len() == 1,
        "Expected one preview surface, got {windows:?}"
    );
    let hwnd = HWND(windows[0] as *mut _);
    unsafe {
        ensure!(
            GetWindow(hwnd, GW_OWNER)?.0 as isize == owner,
            "Preview owner changed"
        );
        ensure!(GetForegroundWindow() != hwnd, "Preview took foreground");
        ensure!(
            GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_NOACTIVATE.0 as isize != 0,
            "Preview lost no-activate style"
        );
        // Synthetic GPUI events do not move the physical cursor. Stop only
        // this native preview's cursor-leave timer after observing it visible.
        let native = GetPropW(HWND(owner as *mut _), w!("ClipHost.NativeHoverPreview")).0 as isize;
        if native == windows[0] {
            let _ = KillTimer(Some(hwnd), 1);
        }
    }
    let mut bounds = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut bounds)?;
    }
    // The GPUI surface includes transparent margins outside its actual paint
    // and input region. Check that region, not the enclosing HWND rectangle.
    // Native layered image previews have no explicit region, so use their
    // complete window rectangle instead.
    let mut region = RECT::default();
    let region_kind = unsafe { GetWindowRgnBox(hwnd, &mut region) };
    let region_bounds = if region_kind.0 >= 2 {
        [
            bounds.left + region.left,
            bounds.top + region.top,
            bounds.left + region.right,
            bounds.top + region.bottom,
        ]
    } else if region_kind.0 == 1 {
        [bounds.left, bounds.top, bounds.left, bounds.top]
    } else {
        [bounds.left, bounds.top, bounds.right, bounds.bottom]
    };
    // Capture and publish geometry before the assertion, so a failed check
    // retains its real pixels and the bounds used by the decision.
    let pixels = capture(windows[0], path)?;
    let clear =
        region_kind.0 == 1 || region_bounds[2] <= beside[0] || region_bounds[0] >= beside[2];
    let detail = json!({"hwnd":windows[0], "owner":owner, "noActivate":true,
        "windowBoundsPx":[bounds.left,bounds.top,bounds.right,bounds.bottom],
        "regionKind":region_kind.0,"regionBoundsPx":region_bounds,
        "regionBoundsSource":if region_kind.0 == 0 {"default-full-window"} else {"GetWindowRgnBox"},
        "besidePx":beside,"avoidsColumns":clear,"screenshot":pixels});
    {
        let mut value = report.lock().unwrap();
        if value["observations"].is_null() {
            value["observations"] = json!({});
        }
        value["observations"][name] = detail.clone();
    }
    ensure!(
        clear,
        "Preview paint/input region overlaps the menu columns: {detail}"
    );
    Ok(detail)
}

pub fn run(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<()> {
    let (mut output, mut theme) = (None, "system".to_owned());
    let mut args = arguments.into_iter();
    while let Some(key) = args.next() {
        if matches!(key.as_str(), "--help" | "-h") {
            println!("super_menu_file_preview_visual --output FRESH_ABSOLUTE_REPO_TMP_DIRECTORY [--theme system|dark|light]");
            return Ok(());
        }
        let value = args
            .next()
            .with_context(|| format!("{key} requires a value"))?;
        match key.as_str() {
            "--output" => output = Some(PathBuf::from(value)),
            "--theme" => {
                ensure!(
                    ["system", "dark", "light"].contains(&value.as_str()),
                    "Invalid theme"
                );
                theme = value;
            }
            _ => return Err(anyhow!("Unknown argument: {key}")),
        }
    }
    let run = output_directory(&output.context("--output is required")?)?;
    let text = run.join("preview.rs");
    let image = run.join("preview.png");
    fs::write(&text, "//! Isolated menu preview sample.\nfn main() {\n    let ready = true;\n    println!(\"{ready}\");\n}\n")?;
    image::RgbImage::from_fn(640, 360, |x, y| {
        image::Rgb([(40 + x * 80 / 640) as u8, (80 + y * 90 / 360) as u8, 140])
    })
    .save(&image)?;
    cliphost_core::force_headless(true);
    let host = Arc::new(
        HostState::open(AppPaths::from_data_root(run.join("data")), 29065)
            .map_err(|error| anyhow!(error))?,
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let hover = Arc::new(
        HoverService::new(host.as_ref().clone(), runtime.handle().clone())
            .map_err(|error| anyhow!(error))?,
    );
    let shutdown = hover.clone();
    let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
    let report = Arc::new(Mutex::new(
        json!({"complete":false,"passed":false,"theme":theme,
        "headless":true,"nativeHostStarted":false,"apiStarted":false,"rpcStarted":false,
        "captureStarted":false,"globalHooksStarted":false,"desktopInputInjected":false}),
    ));
    let completed = report.clone();
    let report_path = run.join("file-preview-report.json");
    let final_path = report_path.clone();
    fs::write(
        &report_path,
        serde_json::to_vec_pretty(&*report.lock().unwrap())?,
    )?;
    Application::new().with_assets(DesignAssets).run(move |cx| {
        crate::hover_card::install_service(hover.clone(), host.clone(), tokens.clone(), cx);
        let view_host = host.clone();
        let view_tokens = tokens.clone();
        let bounds = Bounds::centered(None, size(px(900.), px(600.)), cx);
        let handle = cx.open_window(WindowOptions {
            titlebar:None, focus:false, show:false, is_resizable:false, is_minimizable:false,
            window_bounds:Some(WindowBounds::Windowed(bounds)),
            window_background:gpui::WindowBackgroundAppearance::Transparent, ..Default::default()
        }, move |window, cx| {
            let dark = match theme.as_str() {
                "dark" => true, "light" => false,
                _ => matches!(window.appearance(), gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark),
            };
            cx.new(|cx| SuperMenuWindow::new(view_host, view_tokens, dark, cx))
        }).expect("Open menu preview fixture");
        let setup = handle.update(cx, |menu, window, cx| -> anyhow::Result<isize> {
            platform::set_no_activate(window, true).map_err(|error| anyhow!(error))?;
            let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
            menu.host.set_super_menu_hwnd(hwnd.0 as isize);
            menu.host.menu_create_session_ex("text", "", hwnd.0 as i64, hwnd.0 as i64);
            menu.retire(cx);
            menu.state = menu.host.super_menu_state_detail();
            menu.session = SessionKey::from_state(&menu.state);
            menu.category = Category::File;
            menu.source_hwnd = hwnd.0 as i64;
            menu.state["style"]["menuMaxWidth"] = json!(240.);
            menu.state["style"]["menuFontSize"] = json!(12.);
            menu.state["hostDpiScaleX"] = json!(1.);
            menu.state["hostDpiScaleY"] = json!(1.);
            menu.state["overlayLeftPx"] = json!(0.);
            menu.state["overlayTopPx"] = json!(0.);
            menu.state["anchorScreenX"] = json!(120.);
            menu.state["anchorScreenY"] = json!(150.);
            menu.columns = vec![Column::from_page(String::new(), "children", 150., &json!({
                "status":"Ready", "items":([text,image].iter().enumerate().map(|(index,path)| json!({
                    "id":format!("preview-fixture-{index}"),"kind":"Command","role":"file",
                    "title":path.file_name().unwrap().to_string_lossy(),"isEnabled":true,
                    "hasChildren":false,"hasContextMenu":false,"invokeMode":"None",
                    "payload":{"path":path.display().to_string()}
                })).collect::<Vec<_>>())
            }))];
            menu.file_preview_service = Some(hover.clone());
            menu.file_preview_enabled = true;
            menu.bind_file_preview();
            ensure!(menu.file_preview_owner.is_some() && menu.render_current(&menu.session, menu.value_presentation),
                "Fixture did not bind a valid menu hover session");
            ensure!(menu.keyboard_request.is_none() && menu.native_key_request.is_none(), "Unexpected keyboard binding");
            cx.notify();
            platform::show_no_activate(window).map_err(|error| anyhow!(error))?;
            Ok(hwnd.0 as isize)
        });
        let report_path = report_path.clone();
        let run = run.clone();
        cx.spawn(async move |cx| {
            let outcome = async {
                let owner = setup.map_err(|error| anyhow!(error.to_string()))??;
                Timer::after(Duration::from_millis(300)).await;
                let first = move_row(handle, 0, cx)?;
                let started = Instant::now();
                while started.elapsed() < Duration::from_millis(450) {
                    ensure!(owned_visible(owner).is_empty(), "Cold hover showed before 500ms");
                    Timer::after(Duration::from_millis(25)).await;
                }
                let until = Instant::now() + Duration::from_secs(3);
                while owned_visible(owner).is_empty() {
                    ensure!(Instant::now() < until, "Text preview did not appear");
                    Timer::after(Duration::from_millis(20)).await;
                }
                Timer::after(Duration::from_millis(100)).await;
                let beside = handle.update(cx, |menu, window, _| -> anyhow::Result<[i32; 4]> {
                    let bounds = super::file_preview::avoid_bounds(menu.column_rects.borrow().values().copied())
                        .context("No painted menu columns")?;
                    let rect = Bounds::new(point(px(bounds[0] as f32), px(bounds[1] as f32)),
                        size(px(bounds[2] as f32), px(bounds[3] as f32)));
                    let (left, top, width, height) = platform::client_rect_on_screen(window, rect)
                        .context("Cannot map painted menu columns to screen")?;
                    Ok([left, top, left + width, top + height])
                })??;
                let menu_capture = capture(owner, &run.join("menu.png"))?;
                let text_capture = screenshot(owner, beside, &run.join("text-preview.png"), &report, "text")?;
                let second = move_row(handle, 1, cx)?;
                ensure!(second > first, "Row switch reused the old hover ticket");
                let until = Instant::now() + Duration::from_secs(3);
                let native = unsafe { GetPropW(HWND(owner as *mut _), w!("ClipHost.NativeHoverPreview")).0 as isize };
                while !owned_visible(owner).contains(&native) {
                    ensure!(Instant::now() < until, "Image preview did not appear");
                    Timer::after(Duration::from_millis(20)).await;
                }
                let image_capture = screenshot(owner, beside, &run.join("image-preview.png"), &report, "image")?;
                handle.update(cx, |menu, _, _| menu.cancel_file_preview())?;
                Timer::after(Duration::from_millis(100)).await;
                ensure!(owned_visible(owner).is_empty(), "Cancel left a preview visible");
                let pending = move_row(handle, 0, cx)?;
                handle.update(cx, |menu, window, cx| menu.hide_inactive(window, cx))?;
                Timer::after(Duration::from_millis(650)).await;
                ensure!(owned_visible(owner).is_empty(), "Closed session revived a delayed preview");
                ensure!(handle.update(cx, |menu, _, _| menu.file_preview_owner.is_none())?, "Closed owner lease survived");
                ensure!(unsafe { GetForegroundWindow().0 as isize } != owner, "Menu took foreground");
                Ok::<_, anyhow::Error>(json!({"firstTicket":first,"switchTicket":second,"retiredPendingTicket":pending,
                    "coldDelayLowerBoundMs":450,"menu":menu_capture,"besidePx":beside,
                    "text":text_capture,"image":image_capture,
                    "cancelHidden":true,"closeCanceledPending":true,
                    "scope":"Real GPUI row MouseMove, live hover ledger, own native surfaces; no physical cursor, IME, popup clicks or mixed-DPI acceptance"}))
            }.await;
            let _ = handle.update(cx, |menu, window, cx| menu.hide_inactive(window, cx));
            let mut value = report.lock().unwrap();
            value["complete"] = json!(true);
            match outcome {
                Ok(detail) => { value["passed"] = json!(true); value["detail"] = detail; }
                Err(error) => value["error"] = json!(format!("{error:?}")),
            }
            let _ = fs::write(&report_path, serde_json::to_vec_pretty(&*value).unwrap());
            drop(value);
            let _ = cx.update(|cx| cx.quit());
        }).detach();
    });
    shutdown.shutdown();
    ensure!(
        completed.lock().unwrap()["passed"] == true,
        "Menu preview fixture failed; see {}",
        final_path.display()
    );
    println!("Menu preview fixture passed: {}", final_path.display());
    Ok(())
}
