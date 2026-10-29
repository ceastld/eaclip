//! Exercise the production lazy-window boundary, including open_window's
//! synchronous first draw. The visual stage intentionally bypasses this path.
//! Only fixed nodes and an isolated data root are used; no pointer polling,
//! injected input, clipboard capture, filesystem browsing or core menu session.

use super::{fixture_line, item, level, root, DesignAssets, DesignTokens, FileLine, Sheet, TOKENS_JSON};
use crate::{file_line_trigger::{EdgeTrigger, Sample}, platform};
use anyhow::{anyhow, ensure};
use cliphost_core::{AppPaths, HostState};
use gpui::{div, point, px, size, App, AppContext, Application, AsyncApp, Bounds, Context, Entity,
    IntoElement, Render, Timer, Window, WindowBounds, WindowHandle, WindowOptions};
use serde_json::{json, Value};
use std::{fs, path::PathBuf, sync::{Arc, Mutex}, time::{Duration, Instant}};
use windows::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{CreateRectRgn, DeleteObject, GetWindowRgn, PtInRegion},
    UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::{GetForegroundWindow, GetWindowLongPtrW,
        IsWindowVisible, GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOPMOST, WS_EX_TRANSPARENT}},
};

pub(super) fn run(directory: PathBuf, dark: bool) -> anyhow::Result<()> {
    cliphost_core::force_headless(true);
    let host = Arc::new(HostState::open(AppPaths::from_data_root(directory.join("data")), 29065)
        .map_err(|error| anyhow!(error))?);
    let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
    let path = directory.join("file-line-report.json");
    let report = Arc::new(Mutex::new(json!({"complete":false, "passed":false, "scenario":"lifecycle",
        "pointerPolled":false, "inputInjected":false, "checks":[]})));
    let completed = report.clone();
    let final_path = path.clone();
    fs::write(&path, serde_json::to_vec_pretty(&*report.lock().unwrap())?)?;
    Application::new().with_assets(DesignAssets).run(move |cx| {
        let owner = match open_owner(&host, cx) {
            Ok(owner) => owner,
            Err(error) => {
                let mut value = report.lock().unwrap();
                value["complete"] = json!(true);
                value["error"] = json!(format!("Owner setup: {error:#}"));
                let _ = fs::write(&path, serde_json::to_vec_pretty(&*value).unwrap());
                cx.quit();
                return;
            }
        };
        report.lock().unwrap()["ownerHwnd"] = json!(host.host_hwnd());
        let line = cx.new(|_| {
            let mut line = fixture_line(host, tokens, dark, "peek");
            line.session = None;
            line.trigger.retract();
            line
        });
        cx.spawn(async move |cx| {
            let result = async {
                exercise(&line, cx, &path, &report).await?;
                // Retain and verify the fixture owner through the entire run.
                owner.update(cx, |_, window, _| -> anyhow::Result<()> {
                    let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
                    ensure!(!unsafe { IsWindowVisible(hwnd) }.as_bool(), "Fixture owner became visible");
                    ensure!(unsafe { GetForegroundWindow() } != hwnd, "Fixture owner took focus");
                    Ok(())
                })??;
                Ok::<(), anyhow::Error>(())
            }.await;
            // Closing the application releases only these test-owned windows.
            let mut value = report.lock().unwrap();
            value["complete"] = json!(true);
            value["passed"] = json!(result.is_ok());
            if let Err(error) = result { value["error"] = json!(format!("{error:#}")); }
            let _ = fs::write(&path, serde_json::to_vec_pretty(&*value).unwrap());
            let _ = cx.update(|cx| cx.quit());
        }).detach();
    });
    ensure!(completed.lock().unwrap()["passed"] == true,
        "File line lifecycle failed; see {}", final_path.display());
    Ok(())
}

struct Owner;

impl Render for Owner {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement { div() }
}

fn open_owner(host: &HostState, cx: &mut App) -> anyhow::Result<WindowHandle<Owner>> {
    // Production overlays require an owner on the same process and UI thread.
    // Supply a real test-owned HWND instead of weakening that boundary.
    let owner = cx.open_window(WindowOptions {
        titlebar:None, focus:false, show:false, is_resizable:false, is_minimizable:false,
        window_bounds:Some(WindowBounds::Windowed(Bounds::new(point(px(0.0), px(0.0)), size(px(16.0), px(16.0))))),
        ..Default::default()
    }, |window, cx| {
        window.set_window_title("ClipHost isolated file line owner");
        cx.new(|_| Owner)
    })?;
    let hwnd = owner.update(cx, |_, window, _| platform::hwnd(window).map_err(|error| anyhow!(error)))??;
    ensure!(!unsafe { IsWindowVisible(hwnd) }.as_bool(), "Fixture owner must stay hidden");
    host.set_host_hwnd(hwnd.0 as isize);
    Ok(owner)
}

fn peek(line: &mut FileLine) {
    line.trigger = EdgeTrigger::default();
    let now = Instant::now();
    let sample = |now| Sample { now, x:400, at_edge:true, in_peek_band:true, in_line:false, blocked:false };
    line.trigger.step(sample(now));
    line.trigger.step(sample(now + Duration::from_millis(220)));
}

fn drive(line: &Entity<FileLine>, cx: &AsyncApp, action: impl FnOnce(&mut FileLine, &mut Context<FileLine>)) -> anyhow::Result<()> {
    cx.update(|cx| line.update(cx, action))
}

fn drive_from_line_window(line: &Entity<FileLine>, cx: &AsyncApp,
    action: impl FnOnce(&mut FileLine, &mut Context<FileLine>)) -> anyhow::Result<()> {
    cx.update(|cx| {
        let handle = line.read(cx).line_window.expect("line window already created");
        // Click handlers run with this window already borrowed by GPUI.
        // Nested handle.update would fail even if model reentry were fixed.
        handle.update(cx, |_, _, cx| line.update(cx, action))
    })??;
    Ok(())
}

async fn exercise(line: &Entity<FileLine>, cx: &mut AsyncApp, path: &std::path::Path,
    report: &Arc<Mutex<Value>>) -> anyhow::Result<()> {
    // Both requests happen within one model update. A deferred creation must
    // reread the final phase, so neither stale request can create a window.
    drive(line, cx, |line, cx| {
        peek(line);
        line.sync_windows(cx);
        line.trigger.drop_now(true);
        line.sync_windows(cx);
        line.trigger.retract();
        line.sync_windows(cx);
    })?;
    let cancelled = check(line, cx, "cancelled_before_first_draw", false, false, path, report).await?;
    ensure!(cancelled["line"].is_null() && cancelled["thread"].is_null(), "Cancelled reveal created windows");

    drive(line, cx, |line, cx| { peek(line); line.sync_windows(cx); })?;
    let peeked = check(line, cx, "first_peek_creates_thread", false, true, path, report).await?;
    ensure!(peeked["line"].is_null(), "Peek created the full line early");
    let thread_hwnd = peeked["thread"]["hwnd"].clone();

    drive(line, cx, |line, cx| { line.trigger.drop_now(true); line.sync_windows(cx); })?;
    let down = check(line, cx, "first_drop_creates_line", true, false, path, report).await?;
    let line_hwnd = down["line"]["hwnd"].clone();
    ensure!(down["thread"]["hwnd"] == thread_hwnd, "Drop replaced the thread");

    drive_from_line_window(line, cx, |line, cx| { line.trigger.retract(); line.fold(cx); })?;
    check(line, cx, "fold_hides_both_windows", false, false, path, report).await?;

    drive(line, cx, |line, cx| {
        (line.slots, line.add) = super::line_slots(&root());
        let node = root().into_iter().find(|node| node["id"] == "recent").unwrap();
        let child = item("sample", "Item", "fixture.txt", "file", "Invoke", r"D:\fixture\fixture.txt");
        line.sheets.push(Sheet { tag:"recent".into(), pinned:true,
            levels:vec![level(node, vec![child])], task:None });
        line.trigger.drop_now(true);
        line.sync_windows(cx);
    })?;
    let reopened = check(line, cx, "reopen_reuses_line_with_pin", true, false, path, report).await?;
    ensure!(reopened["line"]["hwnd"] == line_hwnd, "Reopen replaced the line window");

    drive_from_line_window(line, cx, |line, cx| {
        line.toggle_pin("recent", cx);
        line.toggle_pin("recent", cx);
        line.trigger.retract();
        line.fold(cx);
    })?;
    let pinned = check(line, cx, "window_event_pin_and_fold_updates_regions", true, true, path, report).await?;
    ensure!(pinned["line"]["hwnd"] == line_hwnd && pinned["thread"]["hwnd"] == thread_hwnd,
        "Pinned fold replaced windows");
    ensure!(pinned["pinned"] == 1, "Pinned sheet was lost");

    drive(line, cx, |line, cx| { line.suspended = true; line.sync_windows(cx); })?;
    check(line, cx, "suspension_hides_pinned_windows", false, false, path, report).await?;
    drive(line, cx, |line, cx| { line.suspended = false; line.sync_windows(cx); })?;
    check(line, cx, "resume_restores_pinned_windows", true, true, path, report).await?;

    // The isolated config retains the product default (feature disabled).
    // tick returns before querying the pointer and clears all pinned state.
    drive(line, cx, |line, cx| { assert!(!line.host.file_line_enabled()); line.tick(cx); })?;
    let disabled = check(line, cx, "disabled_setting_clears_pins_and_hides", false, false, path, report).await?;
    ensure!(disabled["pinned"] == 0, "Disable retained pinned state");
    Ok(())
}

async fn check(line: &Entity<FileLine>, cx: &AsyncApp, name: &str, line_visible: bool,
    thread_visible: bool, path: &std::path::Path, report: &Arc<Mutex<Value>>) -> anyhow::Result<Value> {
    // Native show/region operations use the window's posted dispatch queue.
    Timer::after(Duration::from_millis(250)).await;
    let snapshot = cx.update(|cx| snapshot(line, cx, line_visible, thread_visible))??;
    let mut value = report.lock().unwrap();
    value["checks"].as_array_mut().unwrap().push(json!({"name":name, "passed":true, "state":snapshot}));
    fs::write(path, serde_json::to_vec_pretty(&*value)?)?;
    Ok(snapshot)
}

fn snapshot(line: &Entity<FileLine>, cx: &mut App, line_visible: bool, thread_visible: bool) -> anyhow::Result<Value> {
    // Release the model read before touching either window.
    let (line_window, thread_window, regions, pinned, phase) = {
        let model = line.read(cx);
        ensure!(model.status.is_none(), "Model error: {:?}", model.status);
        ensure!(model.line_shown == line_visible && model.thread_shown == thread_visible, "Wrong model visibility");
        (model.line_window, model.thread_window, model.regions(),
            model.sheets.iter().filter(|sheet| sheet.pinned).count(), format!("{:?}", model.trigger.phase()))
    };
    let line = window_state(line_window, cx, line_visible, false, &regions)?;
    let thread = window_state(thread_window, cx, thread_visible, true, &[])?;
    Ok(json!({"line":line, "thread":thread, "pinned":pinned, "phase":phase}))
}

fn window_state<V: Render + 'static>(handle: Option<WindowHandle<V>>, cx: &mut App, visible: bool,
    click_through: bool, regions: &[(f32, f32, f32, f32)]) -> anyhow::Result<Value> {
    let Some(handle) = handle else { ensure!(!visible, "Missing visible window"); return Ok(Value::Null); };
    let hwnd = handle.update(cx, |_, window, _| platform::hwnd(window).map_err(|error| anyhow!(error)))??;
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    ensure!(unsafe { IsWindowVisible(hwnd) }.as_bool() == visible, "Wrong native visibility");
    ensure!(unsafe { GetForegroundWindow() } != hwnd, "Fixture took foreground focus");
    if visible {
        ensure!(style & WS_EX_NOACTIVATE.0 as isize != 0, "Visible window can activate");
        ensure!(style & WS_EX_TOPMOST.0 as isize != 0, "Visible window is not topmost");
        if click_through { ensure!(style & WS_EX_TRANSPARENT.0 as isize != 0, "Thread intercepts input"); }
        if !regions.is_empty() { verify_regions(hwnd, regions)?; }
    }
    Ok(json!({"hwnd":hwnd.0 as usize, "visible":visible, "noActivate":style & WS_EX_NOACTIVATE.0 as isize != 0,
        "clickThrough":style & WS_EX_TRANSPARENT.0 as isize != 0, "regionsChecked":if visible { regions.len() } else { 0 }}))
}

fn verify_regions(hwnd: HWND, regions: &[(f32, f32, f32, f32)]) -> anyhow::Result<()> {
    let region = unsafe { CreateRectRgn(0, 0, 0, 0) };
    ensure!(!region.is_invalid(), "CreateRectRgn failed");
    let result = (|| {
        ensure!(unsafe { GetWindowRgn(hwnd, region) }.0 != 0, "Line has no native input region");
        let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
        for &(x, y, width, height) in regions {
            ensure!(unsafe { PtInRegion(region, ((x + width / 2.0) * scale) as i32,
                ((y + height / 2.0) * scale) as i32) }.as_bool(), "Painted content is outside native input region");
        }
        ensure!(!unsafe { PtInRegion(region, (8.0 * scale) as i32, (400.0 * scale) as i32) }.as_bool(),
            "Empty desktop area intercepts input");
        let top_expected = regions.iter().any(|&(x, y, width, height)|
            8.0 >= x && 8.0 < x + width && 8.0 >= y && 8.0 < y + height);
        ensure!(unsafe { PtInRegion(region, (8.0 * scale) as i32, (8.0 * scale) as i32) }.as_bool() == top_expected,
            "Native region retained an outdated dropped-line band");
        Ok(())
    })();
    unsafe { let _ = DeleteObject(region.into()); }
    result
}
