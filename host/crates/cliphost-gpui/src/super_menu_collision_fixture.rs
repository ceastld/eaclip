//! Debug-only hit-testing and handler routing over real SuperMenuWindow rows.
//! Inputs are dispatched inside this GPUI window, never through global input.
//! Fake registry IDs verify UI branches, not actual OS activation or execution.

use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, ensure, Context as _};
use cliphost_core::{AppPaths, HostState};
use gpui::{
    point, prelude::*, px, size, AnyWindowHandle, App, Application, Bounds, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Timer, Window, WindowBounds,
    WindowHandle, WindowOptions,
};
use serde_json::{json, Value};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
};

use super::{
    column_rect, strip_gear_rect, CascadeSide, Category, Column, Rect, SessionKey, SuperMenuWindow,
};
use crate::{
    platform,
    preview::{DesignTokens, TOKENS_JSON},
    DesignAssets,
};

struct Locations {
    run: PathBuf,
    data: PathBuf,
    report: PathBuf,
}

fn locations(output: &Path) -> anyhow::Result<Locations> {
    ensure!(output.is_absolute(), "--output must be absolute");
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .context("Missing repository root")?
        .canonicalize()?;
    let scratch = repository
        .join(".tmp")
        .canonicalize()
        .context("Create repository .tmp before running this fixture")?;
    ensure!(
        scratch.parent() == Some(repository.as_path())
            && scratch.file_name().is_some_and(|name| name == ".tmp"),
        "Repository .tmp must not escape through a junction"
    );
    let parent = output
        .parent()
        .context("Missing output parent")?
        .canonicalize()?;
    ensure!(
        parent.starts_with(&scratch),
        "--output must be inside repository .tmp"
    );
    let run = parent.join(
        output
            .file_name()
            .context("Missing output directory name")?,
    );
    fs::create_dir(&run).context("--output must be a fresh, nonexistent run directory")?;
    let run = run.canonicalize()?;
    ensure!(
        run.starts_with(&scratch) && run != scratch,
        "Output escaped repository .tmp"
    );
    let data = run.join("data");
    fs::create_dir(&data)?;
    let profile = run.join("profile");
    fs::create_dir(&profile)?;
    let roaming = profile.join("Roaming");
    let local = profile.join("Local");
    fs::create_dir(&roaming)?;
    fs::create_dir(&local)?;
    // Set these before HostState or Application can start a worker. Even the
    // headless recent-file snapshot must stay inside this disposable run.
    std::env::set_var("APPDATA", &roaming);
    std::env::set_var("LOCALAPPDATA", &local);
    std::env::set_var("BETTERCLIP_TAURI_DATA_ROOT", &data);
    let report = run.join("collision-report.json");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&report)?;
    Ok(Locations { run, data, report })
}

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    width: f32,
    anchor_x: f32,
    depth: usize,
}

const SCENARIOS: [Scenario; 4] = [
    Scenario {
        name: "two-columns-overlap-right",
        width: 440.,
        anchor_x: 12.,
        depth: 2,
    },
    Scenario {
        name: "two-columns-overlap-left",
        width: 440.,
        anchor_x: 148.,
        depth: 2,
    },
    Scenario {
        name: "three-columns-left-then-right",
        width: 600.,
        anchor_x: 308.,
        depth: 3,
    },
    Scenario {
        name: "three-columns-right-then-left",
        width: 600.,
        anchor_x: 12.,
        depth: 3,
    },
];

fn page(index: usize, count: usize) -> Value {
    json!({"status":"Ready","items":(0..count).map(|row| json!({
        "id":format!("collision-{index}-{row}"), "kind":"Command",
        "title":format!("固定菜单 {} · {}", index + 1, row + 1),
        "isEnabled":true, "hasChildren":false, "hasContextMenu":false,
        "invokeMode":"None", "role":"collision-fixture-leaf"
    })).collect::<Vec<_>>()})
}

fn configure(
    menu: &mut SuperMenuWindow,
    window: &mut Window,
    scenario: Scenario,
    cx: &mut gpui::Context<SuperMenuWindow>,
) -> anyhow::Result<Vec<Rect>> {
    let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
    menu.host.set_super_menu_hwnd(hwnd.0 as isize);
    // Text roots use only this fresh core's settings. An explicit target avoids
    // looking up any external foreground window; query_target sees our HWND.
    menu.host
        .menu_create_session_ex("text", "", hwnd.0 as i64, hwnd.0 as i64);
    menu.retire(cx);
    menu.state = menu.host.super_menu_state_detail();
    menu.session = SessionKey::from_state(&menu.state);
    // Keep the harmless text registry, but render the generic tree. The text
    // category now owns a dedicated launcher and no longer paints columns.
    menu.category = Category::Window;
    menu.source_hwnd = hwnd.0 as i64;
    menu.state["style"]["menuMaxWidth"] = json!(280.);
    menu.state["style"]["menuFontSize"] = json!(13.);
    menu.state["hostDpiScaleX"] = json!(1.);
    menu.state["hostDpiScaleY"] = json!(1.);
    menu.state["overlayLeftPx"] = json!(0.);
    menu.state["overlayTopPx"] = json!(0.);
    menu.state["anchorScreenX"] = json!(scenario.anchor_x);
    menu.state["anchorScreenY"] = json!(320.);
    let size = (
        f32::from(window.viewport_size().width),
        f32::from(window.viewport_size().height),
    );
    ensure!(
        (size.0 - scenario.width).abs() < 1. && (size.1 - 640.).abs() < 1.,
        "Unexpected client size: {size:?}"
    );
    // Same origin as the real render: below the category strip at its left edge.
    let anchor = menu.menu_anchor(size);
    let mut previous = None;
    let mut side = CascadeSide::Right;
    let mut rects = Vec::new();
    for index in 0..scenario.depth {
        let count = if index == 0 {
            10
        } else if index == 1 && scenario.depth == 3 {
            5
        } else {
            2
        };
        let anchor_y = if index == 1 { 390. } else { 430. };
        let column = Column::from_page(
            format!("fixture-parent-{index}"),
            "children",
            anchor_y,
            &page(index, count),
        );
        let (rect, next_side) = column_rect(
            previous,
            side,
            anchor,
            anchor_y,
            280.,
            count as f32 * 28. + 16.,
            size,
        );
        previous = Some(rect);
        side = next_side;
        rects.push(rect);
        menu.columns.push(column);
    }
    ensure!(
        menu.render_current(&menu.session, menu.value_presentation),
        "Real session guard rejected fixture rows"
    );
    ensure!(
        menu.keyboard_request.is_none() && menu.column_tasks.is_empty(),
        "Fixture unexpectedly scheduled host work"
    );
    cx.notify();
    Ok(rects)
}

fn draw(
    handle: WindowHandle<SuperMenuWindow>,
    cx: &mut impl gpui::AppContext,
) -> anyhow::Result<()> {
    AnyWindowHandle::from(handle).update(cx, |_, window, cx| {
        window.refresh();
        window.draw(cx).clear();
    })?;
    Ok(())
}

fn move_and_count(
    handle: WindowHandle<SuperMenuWindow>,
    at: (f32, f32),
    cx: &mut impl gpui::AppContext,
) -> anyhow::Result<usize> {
    AnyWindowHandle::from(handle).update(cx, |root, window, cx| {
        window.dispatch_input(
            PlatformInput::MouseMove(MouseMoveEvent {
                position: point(px(at.0), px(at.1)),
                ..Default::default()
            }),
            cx,
        );
        window.refresh();
        window.draw(cx).clear();
        let menu = root
            .downcast::<SuperMenuWindow>()
            .map_err(|_| anyhow!("Fixture root changed"))?;
        Ok(menu.read(cx).columns.len())
    })?
}

#[derive(Clone, Copy, Debug)]
enum RowGesture {
    Hover,
    Left,
    RepeatedLeft,
    Disclosure,
    ContextButton,
    Right,
}

#[derive(Clone, Copy)]
struct RowCase {
    name: &'static str,
    row: usize,
    gesture: RowGesture,
    invokes: bool,
    collection: Option<&'static str>,
}

const ROW_CASES: [RowCase; 20] = [
    RowCase { name: "window-left-invokes", row: 0, gesture: RowGesture::Left, invokes: true, collection: None },
    RowCase { name: "group-left-invokes", row: 1, gesture: RowGesture::Left, invokes: true, collection: None },
    RowCase { name: "window-hover-operations", row: 0, gesture: RowGesture::Hover, invokes: false, collection: Some("children") },
    RowCase { name: "window-disclosure-operations", row: 0, gesture: RowGesture::Disclosure, invokes: false, collection: Some("children") },
    RowCase { name: "window-right-information", row: 0, gesture: RowGesture::Right, invokes: false, collection: Some("context-menu") },
    RowCase { name: "group-hover-children", row: 1, gesture: RowGesture::Hover, invokes: false, collection: Some("children") },
    RowCase { name: "group-disclosure-children", row: 1, gesture: RowGesture::Disclosure, invokes: false, collection: Some("children") },
    RowCase { name: "group-right-context", row: 1, gesture: RowGesture::Right, invokes: false, collection: Some("context-menu") },
    RowCase { name: "embedded-window-root-hover", row: 2, gesture: RowGesture::Hover, invokes: false, collection: Some("children") },
    RowCase { name: "embedded-window-root-left", row: 2, gesture: RowGesture::Left, invokes: false, collection: Some("children") },
    RowCase { name: "copy-more-left-expands-path", row: 3, gesture: RowGesture::Left, invokes: false, collection: Some("children") },
    RowCase { name: "leaf-context-button", row: 4, gesture: RowGesture::ContextButton, invokes: false, collection: Some("context-menu") },
    RowCase { name: "leaf-right-context", row: 4, gesture: RowGesture::Right, invokes: false, collection: Some("context-menu") },
    RowCase { name: "disabled-window-left-ignored", row: 5, gesture: RowGesture::Left, invokes: false, collection: None },
    RowCase { name: "disabled-window-disclosure-ignored", row: 5, gesture: RowGesture::Disclosure, invokes: false, collection: None },
    RowCase { name: "disabled-window-hover-ignored", row: 5, gesture: RowGesture::Hover, invokes: false, collection: None },
    RowCase { name: "disabled-window-right-information", row: 5, gesture: RowGesture::Right, invokes: false, collection: Some("context-menu") },
    RowCase { name: "recent-none-with-payload-path-expands", row: 6, gesture: RowGesture::Left, invokes: false, collection: Some("children") },
    RowCase { name: "opened-none-with-description-path-expands", row: 7, gesture: RowGesture::Left, invokes: false, collection: Some("children") },
    RowCase { name: "window-second-click-count-ignored", row: 0, gesture: RowGesture::RepeatedLeft, invokes: false, collection: None },
];

fn interaction_page() -> Value {
    let window = json!({"id":"handler-fixture-window", "title":"Fixture window", "role":"window",
        "kind":"Folder", "hasChildren":true, "hasContextMenu":true, "isEnabled":true,
        "invokeMode":"InvokeAndExpand", "payload":{"windowHandle":42}});
    let mut group = window.clone();
    group["id"] = json!("handler-fixture-group");
    group["title"] = json!("Fixture group");
    group["role"] = json!("window-group");
    let mut root = window.clone();
    root["id"] = json!("handler-fixture-window-root");
    root["title"] = json!("Embedded window root");
    root["invokeMode"] = json!("None");
    root["payload"]["root"] = json!(true);
    let mut disabled = window.clone();
    disabled["id"] = json!("handler-fixture-disabled");
    disabled["title"] = json!("Disabled fixture window");
    disabled["isEnabled"] = json!(false);
    json!({"status":"Ready", "items":[window, group, root,
        {"id":"handler-fixture-copy-more", "title":"Copy more", "role":"file-copy-more",
            "kind":"Folder", "hasChildren":true, "hasContextMenu":false, "isEnabled":true,
            "invokeMode":"Expand", "payload":{"path":"D:\\handler-fixture\\never-open.txt"}},
        {"id":"handler-fixture-leaf", "title":"Leaf with more actions", "role":"handler-fixture-leaf",
            "kind":"Command", "hasChildren":false, "hasContextMenu":true, "isEnabled":true,
            "invokeMode":"Invoke"}, disabled,
        {"id":"handler-fixture-recent-root", "title":"Recent container", "role":"recent",
            "kind":"Folder", "hasChildren":true, "hasContextMenu":false, "isEnabled":true,
            "invokeMode":"None", "payload":{"path":"D:\\handler-fixture\\recent"}},
        {"id":"handler-fixture-opened-root", "title":"Opened container", "role":"opened-folder",
            "kind":"Folder", "hasChildren":true, "hasContextMenu":false, "isEnabled":true,
            "invokeMode":"None", "description":"D:\\handler-fixture\\opened"}]})
}

fn row_case(
    handle: WindowHandle<SuperMenuWindow>,
    case: RowCase,
    cx: &mut impl gpui::AppContext,
) -> anyhow::Result<Value> {
    handle.update(cx, |menu, window, cx| -> anyhow::Result<()> {
        configure(menu, window, Scenario {
            name: "handler-routing", width: 900., anchor_x: 12., depth: 1,
        }, cx)?;
        menu.columns = vec![Column::from_page(String::new(), "children", 0., &interaction_page())];
        cx.notify();
        Ok(())
    })??;
    draw(handle, cx)?;
    // Stable row IDs retain hover state between cases. Leave all rows before
    // dispatching the next gesture, while they still have no pending work.
    move_and_count(handle, (1., 1.), cx)?;
    let observation = AnyWindowHandle::from(handle).update(cx, |root, window, cx| {
        let entity = root.downcast::<SuperMenuWindow>()
            .map_err(|_| anyhow!("Fixture root changed"))?;
        let bounds = entity.read(cx).row_bounds.borrow().get(&(0, case.row)).copied()
            .context("Handler fixture row was not painted")?;
        let at = if matches!(case.gesture, RowGesture::Disclosure | RowGesture::ContextButton) {
            point(bounds.right() - px(21.), bounds.center().y)
        } else {
            point(bounds.left() + px(40.), bounds.center().y)
        };
        if matches!(case.gesture, RowGesture::Hover) {
            window.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
                position: at, ..Default::default()
            }), cx);
        } else {
            let button = if matches!(case.gesture, RowGesture::Right) {
                MouseButton::Right
            } else {
                MouseButton::Left
            };
            let click_count = if matches!(case.gesture, RowGesture::RepeatedLeft) { 2 } else { 1 };
            window.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
                position: at, button, click_count, ..Default::default()
            }), cx);
            window.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
                position: at, button, click_count, ..Default::default()
            }), cx);
        }
        let menu = entity.read(cx);
        let observation = json!({"name":case.name,"gesture":format!("{:?}", case.gesture),
            "row":case.row,"point":[f32::from(at.x), f32::from(at.y)],
            "busy":menu.busy,"actionPending":menu.action_task.is_some(),
            "columnCount":menu.columns.len(),
            "collection":menu.columns.get(1).map(|column| column.collection),
            "parent":menu.columns.get(1).map(|column| column.parent.clone()),
            "expectedInvokes":case.invokes,"expectedCollection":case.collection,
            "realRegistrySession":menu.session.as_ref().is_some_and(|key| menu.current(key)),
            "sourceHwnd":menu.source_hwnd,
            "scope":"UI handler routing only; fake registry IDs; OS activation not tested"});
        // Retire before returning to the event loop, including failed cases.
        // Even a worker that starts early can only reject our unregistered IDs.
        entity.update(cx, |menu, cx| { menu.retire(cx); cx.notify(); });
        Ok::<_, anyhow::Error>(observation)
    })??;
    let invokes = observation["busy"] == true && observation["actionPending"] == true;
    ensure!(observation["realRegistrySession"] == true, "{} lost its real core session", case.name);
    ensure!(invokes == case.invokes, "{} invoked={invokes}, expected {}: {observation}", case.name, case.invokes);
    ensure!(observation["collection"].as_str() == case.collection,
        "{} opened an unexpected collection: {observation}", case.name);
    ensure!(case.collection.is_none_or(|_| observation["columnCount"] == 2),
        "{} failed to create exactly one child column: {observation}", case.name);
    if !case.invokes {
        ensure!(observation["actionPending"] == false && observation["busy"] == false,
            "{} accidentally scheduled a command: {observation}", case.name);
    }
    Ok(observation)
}

fn start_setting_click(
    handle: WindowHandle<SuperMenuWindow>,
    control: bool,
    cx: &mut impl gpui::AppContext,
) -> anyhow::Result<()> {
    handle.update(cx, |menu, window, cx| -> anyhow::Result<()> {
        configure(menu, window, Scenario {
            name: "successful-setting-click", width: 900., anchor_x: 12., depth: 1,
        }, cx)?;
        let mut node = menu.state["root"]["items"].as_array()
            .and_then(|items| items.iter().find(|node| node["role"] == "setting"))
            .cloned().context("Fresh text registry has no settings command")?;
        // This case tests successful command replies and Ctrl ownership. The
        // production tree now deliberately strips its old trailing settings
        // row, so give this harmless registry-backed command a fixture role.
        node["role"] = json!("fixture-successful-command");
        menu.columns = vec![Column::from_page(String::new(), "children", 0.,
            &json!({"status":"Ready", "items":[node]}))];
        menu.status.clear();
        menu.host.take_pending_nav("main");
        cx.notify();
        Ok(())
    })??;
    draw(handle, cx)?;
    move_and_count(handle, (1., 1.), cx)?;
    AnyWindowHandle::from(handle).update(cx, |root, window, cx| -> anyhow::Result<()> {
        let entity = root.downcast::<SuperMenuWindow>()
            .map_err(|_| anyhow!("Fixture root changed"))?;
        let bounds = entity.read(cx).row_bounds.borrow().get(&(0, 0)).copied()
            .context("Settings fixture row was not painted")?;
        let at = point(bounds.left() + px(40.), bounds.center().y);
        let modifiers = gpui::Modifiers { control, ..Default::default() };
        window.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
            position: at, button: MouseButton::Left, click_count: 1, modifiers,
            ..Default::default()
        }), cx);
        window.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
            position: at, button: MouseButton::Left, click_count: 1, modifiers,
            ..Default::default()
        }), cx);
        ensure!(entity.read(cx).action_task.is_some(), "Settings click did not reach invoke");
        Ok(())
    })??;
    Ok(())
}

fn finish_setting_click(
    handle: WindowHandle<SuperMenuWindow>,
    control: bool,
    cx: &mut impl gpui::AppContext,
) -> anyhow::Result<Value> {
    let observation = handle.update(cx, |menu, _, cx| {
        let observation = json!({"name":if control { "ctrl-click-keeps-menu-after-success" } else { "plain-click-closes-menu-after-success" },
            "control":control,"actionPending":menu.action_task.is_some(),"busy":menu.busy,
            "sessionCurrent":menu.session.as_ref().is_some_and(|key| menu.current(key)),
            "hostVisible":menu.host.super_menu_state_detail()["visible"],
            "status":menu.status,"mainRoute":menu.host.take_pending_nav("main"),
            "scope":"Real headless settings command and GPUI reply; no displayed settings window or OS activation"});
        menu.retire(cx);
        cx.notify();
        observation
    })?;
    ensure!(observation["actionPending"] == false && observation["busy"] == false,
        "Settings handler failed to complete: {observation}");
    ensure!(observation["mainRoute"] == "/main/settings/textAction" && observation["status"] == "",
        "Settings command did not complete successfully: {observation}");
    ensure!(observation["sessionCurrent"] == control && observation["hostVisible"] == control,
        "Successful click did not preserve the expected menu visibility: {observation}");
    Ok(observation)
}

fn dispatch_left_click(
    handle: WindowHandle<SuperMenuWindow>,
    at: gpui::Point<gpui::Pixels>,
    cx: &mut impl gpui::AppContext,
) -> anyhow::Result<()> {
    AnyWindowHandle::from(handle).update(cx, |_, window, cx| {
        window.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
            position: at, button: MouseButton::Left, click_count: 1, ..Default::default()
        }), cx);
        window.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
            position: at, button: MouseButton::Left, click_count: 1, ..Default::default()
        }), cx);
    })?;
    Ok(())
}

fn open_menu_settings(
    handle: WindowHandle<SuperMenuWindow>,
    cx: &mut impl gpui::AppContext,
) -> anyhow::Result<()> {
    let at = handle.update(cx, |menu, window, cx| -> anyhow::Result<_> {
        configure(menu, window, Scenario {
            name: "in-menu-settings", width: 900., anchor_x: 12., depth: 1,
        }, cx)?;
        menu.settings_switch_bounds.borrow_mut().clear();
        let size = (f32::from(window.viewport_size().width), f32::from(window.viewport_size().height));
        let gear = strip_gear_rect(menu.strip_rect(size));
        Ok(point(px(gear.x + gear.width / 2.), px(gear.y + gear.height / 2.)))
    })??;
    draw(handle, cx)?;
    dispatch_left_click(handle, at, cx)?;
    ensure!(handle.update(cx, |menu, _, _| menu.settings_open && !menu.settings_general)?,
        "Actual settings gear click did not open the window-category panel");
    Ok(())
}

fn click_thumbnail_setting(
    handle: WindowHandle<SuperMenuWindow>,
    cx: &mut impl gpui::AppContext,
) -> anyhow::Result<Value> {
    draw(handle, cx)?;
    let (at, before) = handle.update(cx, |menu, _, _| -> anyhow::Result<_> {
        let key = menu.session.as_ref().context("No settings menu session")?;
        let rect = menu.settings_switch_bounds.borrow().get(&1).copied()
            .context("Window-thumbnail switch was not painted")?;
        let value = menu.settings_values.get(&("menu_global", "ShowWindowThumbnail".to_string()))
            .and_then(Value::as_bool).context("Missing live thumbnail setting")?;
        Ok((rect.center(), json!({"sessionId":key.id,"generation":key.generation,"expected":!value})))
    })??;
    dispatch_left_click(handle, at, cx)?;
    ensure!(handle.update(cx, |menu, _, _| menu.busy && menu.category_task.is_some())?,
        "Actual switch click did not schedule the menu rebuild");
    Ok(before)
}

fn finish_thumbnail_setting(
    handle: WindowHandle<SuperMenuWindow>,
    before: &Value,
    config_path: &Path,
    cx: &mut impl gpui::AppContext,
) -> anyhow::Result<Value> {
    let persisted: Value = serde_json::from_slice(&fs::read(config_path)?)?;
    let observation = handle.update(cx, |menu, _, _| {
        let key = menu.session.as_ref();
        json!({"name":"in-menu-switch-persists-and-rebuilds",
            "expected":before["expected"],"persisted":persisted["ShowWindowThumbnail"],
            "style":menu.state["style"]["windowThumbnail"],
            "displayedValue":menu.settings_values.get(&("menu_global", "ShowWindowThumbnail".to_string())),
            "sessionChanged":key.is_some_and(|key| key.id != before["sessionId"].as_str().unwrap_or("")
                || Some(i64::from(key.generation)) != before["generation"].as_i64()),
            "sessionCurrent":key.is_some_and(|key| menu.current(key)),
            "settingsOpen":menu.settings_open,"generalTab":menu.settings_general,
            "category":menu.category.action(),"busy":menu.busy,"status":menu.status,
            "scope":"Actual hidden GPUI gear/switch events, settings form, disk JSON and rebuilt core session; no global input or window capture"})
    })?;
    ensure!(observation["persisted"] == before["expected"]
        && observation["style"] == before["expected"]
        && observation["displayedValue"] == before["expected"],
        "Settings disk/style/panel values disagree: {observation}");
    ensure!(observation["sessionChanged"] == true && observation["sessionCurrent"] == true
        && observation["settingsOpen"] == true && observation["generalTab"] == false
        && observation["category"] == "window" && observation["busy"] == false
        && observation["status"] == "", "Menu rebuild lost its settings state: {observation}");
    handle.update(cx, |menu, _, cx| { menu.retire(cx); cx.notify(); })?;
    Ok(observation)
}

fn clip_delete_acceptance_case(
    handle: WindowHandle<SuperMenuWindow>,
    cx: &mut impl gpui::AppContext,
) -> anyhow::Result<Value> {
    let observation = handle.update(cx, |menu, window, cx| -> anyhow::Result<Value> {
        configure(menu, window, Scenario {
            name: "clip-delete-acceptance", width: 900., anchor_x: 12., depth: 1,
        }, cx)?;
        menu.category = Category::Clipboard;
        menu.clips = vec![json!({"id":"handler-fixture-never-delete", "kind":"TextClipItem",
            "displayText":"Synthetic row; never stored in the database"})];
        menu.selected = 0;
        menu.clips_total = 2;
        menu.clips_next_skip = 1;
        menu.clips_more = true;
        menu.clips_more_task = Some(gpui::Task::ready(()));
        menu.clip_action("delete", cx);
        let retired_on_accept = menu.clips_more_task.is_none();
        let accepted = menu.busy && menu.action_task.is_some();
        menu.load_more_clips(cx);
        let observation = json!({"name":"clip-delete-retires-page-before-mutation",
            "realRegistrySession":menu.session.as_ref().is_some_and(|key| menu.current(key)),
            "sourceHwnd":menu.source_hwnd,"deleteAccepted":accepted,
            "pageRetiredOnAccept":retired_on_accept,
            "pageStayedRetiredWhileBusy":menu.clips_more_task.is_none(),
            "actionStillPending":menu.action_task.is_some(),"busy":menu.busy,
            "nextSkip":menu.clips_next_skip,"loadedRows":menu.clips.len(),
            "scope":"Real clip_action acceptance and busy guard only; retire in the same UI turn before polling the delete task; no database deletion result"});
        // This foreground task has not had an event-loop turn to run. Drop it
        // here so the fixture never reaches a database mutation or its reply.
        menu.retire(cx);
        cx.notify();
        Ok(observation)
    })??;
    ensure!(observation["realRegistrySession"] == true && observation["deleteAccepted"] == true,
        "Synthetic delete did not reach the accepted action path: {observation}");
    ensure!(observation["pageRetiredOnAccept"] == true && observation["pageStayedRetiredWhileBusy"] == true
        && observation["actionStillPending"] == true && observation["busy"] == true,
        "Delete acceptance left or restarted pagination: {observation}");
    ensure!(observation["nextSkip"] == 1 && observation["loadedRows"] == 1,
        "Delete acceptance ran the result handler prematurely: {observation}");
    Ok(observation)
}

fn foreground() -> usize {
    unsafe { GetForegroundWindow().0 as usize }
}

fn isolation(
    host: &HostState,
    handle: WindowHandle<SuperMenuWindow>,
    before: usize,
    cx: &App,
) -> anyhow::Result<Value> {
    handle.read_with(cx, |menu, _| {
        ensure!(
            menu.column_tasks.is_empty()
                && menu.action_task.is_none()
                && menu.keyboard_request.is_none()
                && menu.native_key_request.is_none(),
            "Unexpected host tasks"
        );
        Ok(())
    })??;
    let actual = host.lan_status();
    ensure!(
        cliphost_core::is_headless()
            && actual["enabled"] == false
            && actual["running"] == false
            && host.lan_listener_port().is_none(),
        "Core isolation invariant failed"
    );
    ensure!(foreground() == before, "Foreground HWND changed");
    Ok(
        json!({"headless":true,"lanStarted":false,"apiStarted":false,"rpcStarted":false,
        "nativeHostStarted":false,"captureStarted":false,"hooksStarted":false,"trayStarted":false,
        "foregroundBefore":before,"foregroundAfter":foreground(),"foregroundPreserved":true}),
    )
}

fn write_report(path: &Path, report: &Value) -> anyhow::Result<()> {
    fs::write(path, serde_json::to_vec_pretty(report)?)?;
    Ok(())
}

pub fn run(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<()> {
    let mut args = arguments.into_iter();
    let first = args
        .next()
        .context("Usage: super_menu_collision --output <fresh-absolute-repo-.tmp-directory>")?;
    if matches!(first.as_str(), "--help" | "-h") {
        println!("Usage: super_menu_collision --output <fresh-absolute-repo-.tmp-directory>\nIn-process GPUI hover/click handler routing; no physical/global input or actual OS activation.");
        return Ok(());
    }
    ensure!(first == "--output", "Expected --output");
    let locations = locations(&PathBuf::from(
        args.next().context("--output requires a directory")?,
    ))?;
    ensure!(args.next().is_none(), "Unexpected extra argument");
    let initial = json!({"mode":"super-menu-collision-fixture","passed":false,"complete":false,
        "pid":std::process::id(),"windowHidden":true,"physicalMouseVerified":false,
        "osActivationVerified":false,"uiHandlerRoutingVerified":false,
        "input":"in-process GPUI MouseMove/MouseDown/MouseUp; fake registry IDs; no physical/global input",
        "runRoot":locations.run,"dataRoot":locations.data,"cases":[],"interactionCases":[],
        "acceptanceCases":[],"successfulReplyCases":[],"settingsCases":[]});
    write_report(&locations.report, &initial)?;
    cliphost_core::force_headless(true);
    let settings_file = locations.data.join("Configs/GlobalMenuConfig.json");
    let setup = (|| -> anyhow::Result<_> {
        let host = Arc::new(
            HostState::open(AppPaths::from_data_root(locations.data), 29065)
                .map_err(|error| anyhow!(error))?,
        );
        let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
        Ok((host, tokens))
    })();
    let (host, tokens) = match setup {
        Ok(value) => value,
        Err(error) => {
            let mut report = initial;
            report["complete"] = json!(true);
            report["error"] = json!(format!("{error:#}"));
            write_report(&locations.report, &report)?;
            return Err(error);
        }
    };
    let before = foreground();
    let result = Arc::new(Mutex::new(initial));
    let completed = result.clone();
    let report_path = locations.report.clone();
    Application::new().with_assets(DesignAssets).run(move |cx| {
        let configured = (|| -> anyhow::Result<WindowHandle<SuperMenuWindow>> {
            let bounds = Bounds::centered(None, size(px(440.), px(640.)), cx);
            let view_host = host.clone();
            let handle = cx.open_window(WindowOptions {
                titlebar: None, focus: false, show: false, is_resizable: false, is_minimizable: false,
                window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default()
            }, move |_, cx| cx.new(|cx| SuperMenuWindow::new(view_host, tokens, true, cx)))?;
            handle.update(cx, |_, window, _| -> anyhow::Result<()> {
                platform::set_no_activate(window, true).map_err(|error| anyhow!(error))?;
                platform::set_geometry(window, None, Some((440., 640.))).map_err(|error| anyhow!(error))?;
                // Keep the native HWND hidden. Manual draw still produces real
                // hitboxes, without intercepting the user's physical pointer.
                Ok(())
            })??;
            Ok(handle)
        })();
        let handle = match configured {
            Ok(handle) => handle,
            Err(error) => {
                let mut report = result.lock().unwrap();
                report["error"] = json!(format!("{error:#}"));
                report["complete"] = json!(true);
                let _ = write_report(&report_path, &report);
                cx.quit();
                return;
            }
        };
        cx.spawn(async move |cx| {
            let outcome = async {
                let mut cases = Vec::new();
                for scenario in SCENARIOS {
                    handle.update(cx, |_, window, _| platform::set_geometry(window, None,
                        Some((f64::from(scenario.width), 640.))).map_err(|error| anyhow!(error)))??;
                    Timer::after(Duration::from_millis(100)).await;
                    let rects = handle.update(cx, |menu, window, cx| configure(menu, window, scenario, cx))??;
                    draw(handle, cx)?;
                    Timer::after(Duration::from_millis(160)).await;
                    draw(handle, cx)?;
                    // Reset hover state shared by stable production row IDs.
                    move_and_count(handle, (1., 1.), cx)?;
                    let top = *rects.last().context("No fixture columns")?;
                    let ancestor = rects[0];
                    let left = top.x.max(ancestor.x);
                    let right = (top.x + top.width).min(ancestor.x + ancestor.width);
                    ensure!(right - left > 20., "Scenario did not overlap");
                    let x = (left + right) * 0.5;
                    let row_point = (x, top.y + 14.);
                    let padding_point = (x, top.y + 2.);
                    let visible_parent_point = (ancestor.x + 20., ancestor.y + 14.);
                    let row_count = move_and_count(handle, row_point, cx)?;
                    let padding_count = move_and_count(handle, padding_point, cx)?;
                    let parent_count = move_and_count(handle, visible_parent_point, cx)?;
                    let checked = cx.update(|cx| -> anyhow::Result<Value> {
                        AnyWindowHandle::from(handle).update(cx, |_, window, _| -> anyhow::Result<()> {
                            let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
                            ensure!(hwnd.0 as usize != before && foreground() != hwnd.0 as usize, "Fixture became foreground");
                            ensure!(unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } & WS_EX_NOACTIVATE.0 as isize != 0,
                                "Fixture lost WS_EX_NOACTIVATE");
                            Ok(())
                        })??;
                        isolation(&host, handle, before, cx)
                    })??;
                    let case = json!({"name":scenario.name,"width":scenario.width,"height":640,
                        "depth":scenario.depth,"rects":rects.iter().map(|r|json!({"x":r.x,"y":r.y,"width":r.width,"maxHeight":r.height})).collect::<Vec<_>>(),
                        "rowPoint":row_point,"paddingPoint":padding_point,"visibleParentPoint":visible_parent_point,
                        "afterChildRow":row_count,"afterChildPadding":padding_count,"afterVisibleParent":parent_count,
                        "rowProtected":row_count==scenario.depth,"paddingProtected":padding_count==scenario.depth,
                        "positiveControl":parent_count==1,"isolation":checked});
                    let passed = row_count == scenario.depth && padding_count == scenario.depth && parent_count == 1;
                    cases.push(case);
                    {
                        let mut report = result.lock().unwrap();
                        report["cases"] = json!(cases);
                        write_report(&report_path, &report)?;
                    }
                    ensure!(passed, "{} failed: child row={row_count}, padding={padding_count}, visible parent={parent_count}", scenario.name);
                }
                handle.update(cx, |_, window, _| platform::set_geometry(window, None,
                    Some((900., 640.))).map_err(|error| anyhow!(error)))??;
                Timer::after(Duration::from_millis(100)).await;
                let mut interaction_cases = Vec::new();
                for case in ROW_CASES {
                    let mut checked = row_case(handle, case, cx)?;
                    checked["isolation"] = cx.update(|cx| isolation(&host, handle, before, cx))??;
                    checked["passed"] = json!(true);
                    interaction_cases.push(checked);
                    let mut report = result.lock().unwrap();
                    report["interactionCases"] = json!(interaction_cases);
                    write_report(&report_path, &report)?;
                }
                let mut deletion = clip_delete_acceptance_case(handle, cx)?;
                deletion["isolation"] = cx.update(|cx| isolation(&host, handle, before, cx))??;
                deletion["passed"] = json!(true);
                {
                    let mut report = result.lock().unwrap();
                    report["acceptanceCases"] = json!([deletion]);
                    write_report(&report_path, &report)?;
                }
                let mut successful_replies = Vec::new();
                for control in [true, false] {
                    start_setting_click(handle, control, cx)?;
                    for _ in 0..100 {
                        Timer::after(Duration::from_millis(50)).await;
                        if !handle.update(cx, |menu, _, _| menu.action_task.is_some())? {
                            break;
                        }
                    }
                    let mut checked = finish_setting_click(handle, control, cx)?;
                    checked["isolation"] = cx.update(|cx| isolation(&host, handle, before, cx))??;
                    checked["passed"] = json!(true);
                    successful_replies.push(checked);
                    let mut report = result.lock().unwrap();
                    report["successfulReplyCases"] = json!(successful_replies);
                    write_report(&report_path, &report)?;
                }
                open_menu_settings(handle, cx)?;
                Timer::after(Duration::from_millis(200)).await;
                let previous = click_thumbnail_setting(handle, cx)?;
                for _ in 0..100 {
                    Timer::after(Duration::from_millis(50)).await;
                    if !handle.update(cx, |menu, _, _| menu.category_task.is_some())? {
                        break;
                    }
                }
                draw(handle, cx)?;
                let mut checked = finish_thumbnail_setting(handle, &previous, &settings_file, cx)?;
                checked["isolation"] = cx.update(|cx| isolation(&host, handle, before, cx))??;
                checked["passed"] = json!(true);
                {
                    let mut report = result.lock().unwrap();
                    report["settingsCases"] = json!([checked]);
                    write_report(&report_path, &report)?;
                }
                result.lock().unwrap()["uiHandlerRoutingVerified"] = json!(true);
                Ok::<(), anyhow::Error>(())
            }.await;
            {
                let mut report = result.lock().unwrap();
                report["complete"] = json!(true);
                report["passed"] = json!(outcome.is_ok());
                report["foregroundBefore"] = json!(before);
                report["foregroundAfter"] = json!(foreground());
                report["foregroundPreserved"] = json!(foreground() == before);
                if let Err(error) = outcome { report["error"] = json!(format!("{error:#}")); }
                if let Err(error) = write_report(&report_path, &report) { eprintln!("Fixture report failed: {error:#}"); }
            }
            let _ = cx.update(|cx| cx.quit());
        }).detach();
    });
    let report = completed.lock().unwrap();
    ensure!(
        report["passed"] == true && report["complete"] == true,
        "Collision regression failed; see {}: {}",
        locations.report.display(),
        report["error"]
    );
    println!(
        "4 collision/hover, 20 UI handler routing, 1 delete acceptance, 2 successful replies and 1 persisted menu-setting case passed (OS activation not tested): {}",
        locations.report.display()
    );
    Ok(())
}
