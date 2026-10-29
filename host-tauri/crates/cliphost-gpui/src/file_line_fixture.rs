//! Fixed tags and sheets through the production line views; no pointer
//! polling, no core session, no injected input. The interaction scenario calls
//! the production model handlers directly. The overlay's transparency is shown
//! over a plain backdrop in one opaque, never-activating window.

use super::{line_slots, FileLine, Level, LineView, Sheet, ThreadView};
use crate::{
    file_line_trigger::{EdgeTrigger, Sample},
    platform,
    preview::{DesignTokens, TOKENS_JSON},
    super_menu::style_fixture::{capture, output_directory},
    view::color,
    DesignAssets,
};
use anyhow::{anyhow, ensure, Context as _};
use cliphost_core::{edge_hide::Rect, screen_edge::ScreenEdge, AppPaths, HostState};
use gpui::{div, prelude::*, px, size, Application, Bounds, Entity, Task, Timer, WindowBounds, WindowOptions};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE};

#[path = "file_line_lifecycle_fixture.rs"]
mod lifecycle;

const WIDTH: f32 = 1180.0;
const HEIGHT: f32 = 460.0;

fn item(id: &str, kind: &str, title: &str, role: &str, mode: &str, path: &str) -> Value {
    json!({"id": id, "kind": kind, "title": title, "role": role, "isEnabled": true,
        "hasChildren": kind == "Folder", "invokeMode": mode, "payload": {"path": path}})
}

fn separator(id: &str) -> Value {
    json!({"id": id, "kind": "Separator", "title": "", "isEnabled": true})
}

fn root() -> Vec<Value> {
    vec![
        item("add", "Item", "添加目录…", "file-add-dir", "Invoke", ""),
        item("add-opened", "Folder", "添加已打开的文件夹…", "file-add-opened-root", "None", ""),
        separator("s1"),
        item("opened", "Folder", "打开的文件夹", "opened-folder", "None", ""),
        separator("s2"),
        item("project", "Folder", "项目", "file", "InvokeAndExpand", r"D:\项目"),
        item("material", "Folder", "项目资料", "file", "InvokeAndExpand", r"C:\Users\demo\Documents\项目资料"),
        item("tools", "Folder", "工具", "file", "InvokeAndExpand", r"C:\工具"),
        separator("s3"),
        item("c", "Folder", "Windows-SSD (C:)", "file", "InvokeAndExpand", r"C:\"),
        item("d", "Folder", "新加卷 (D:)", "file", "InvokeAndExpand", r"D:\"),
        item("g", "Folder", "云端硬盘 (G:)", "file", "InvokeAndExpand", r"G:\"),
        separator("s4"),
        item("recent", "Folder", "最近使用的文件", "recent-files", "None", r"C:\Users\demo\AppData\Roaming\Microsoft\Windows\Recent"),
        separator("s5"),
        item("settings", "Item", "设置", "setting", "Invoke", ""),
    ]
}

fn level(node: Value, items: Vec<Value>) -> Level {
    Level { node, items, cursor: None, has_more: false, loading: false, error: None, scroll: gpui::ScrollHandle::new() }
}

pub fn run(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<()> {
    let (mut output, mut dark, mut scenario) = (None, true, "down".to_string());
    let mut args = arguments.into_iter();
    while let Some(key) = args.next() {
        if matches!(key.as_str(), "--help" | "-h") {
            println!("file_line_visual --output FRESH_ABSOLUTE_REPO_TMP_DIRECTORY [--theme dark|light] [--scenario down|peek|loose|interaction|tray|undo|lifecycle]\nFixed tags; no pointer polling or injected input. Lifecycle exercises production lazy windows; other scenarios use an opaque visual stage.");
            return Ok(());
        }
        let value = args.next().with_context(|| format!("{key} requires a value"))?;
        match key.as_str() {
            "--output" => output = Some(PathBuf::from(value)),
            "--theme" => { ensure!(["dark", "light"].contains(&value.as_str()), "Invalid theme"); dark = value == "dark"; }
            "--scenario" => { ensure!(["down", "peek", "loose", "interaction", "tray", "undo", "lifecycle"].contains(&value.as_str()), "Invalid scenario"); scenario = value; }
            _ => return Err(anyhow!("Unknown argument: {key}")),
        }
    }
    let run = output_directory(&output.context("--output is required")?)?;
    if scenario == "lifecycle" {
        return lifecycle::run(run, dark);
    }
    let report_path = run.join("file-line-report.json");
    let screenshot = run.join("file-line.png");
    cliphost_core::force_headless(true);
    let host = Arc::new(HostState::open(AppPaths::from_data_root(run.join("data")), 29065).map_err(|error| anyhow!(error))?);
    let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
    let before = unsafe { GetForegroundWindow().0 as usize };
    let report = Arc::new(Mutex::new(json!({"complete": false, "passed": false, "theme": if dark { "dark" } else { "light" }, "scenario": scenario})));
    let completed = report.clone();
    let final_path = report_path.clone();
    fs::write(&report_path, serde_json::to_vec_pretty(&*report.lock().unwrap())?)?;
    Application::new().with_assets(DesignAssets).run(move |cx| {
        let line = cx.new(|_| fixture_line(host.clone(), tokens.clone(), dark, &scenario));
        let bounds = Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx);
        let stage_line = line.clone();
        let handle = cx.open_window(WindowOptions { titlebar: None, focus: false, show: false, is_resizable: false, is_minimizable: false,
            window_bounds: Some(WindowBounds::Windowed(bounds)), window_background: gpui::WindowBackgroundAppearance::Opaque, ..Default::default()
        }, move |_, cx| {
            let thread = cx.new(|cx| ThreadView::new(stage_line.clone(), cx));
            let overlay = cx.new(|cx| LineView::new(stage_line.clone(), cx));
            cx.new(|_| Stage { line: stage_line, thread, overlay })
        }).expect("Open isolated file line fixture");
        let setup = handle.update(cx, |_, window, _| -> anyhow::Result<()> {
            platform::set_no_activate(window, true).map_err(|error| anyhow!(error))?;
            platform::set_geometry(window, None, Some((f64::from(WIDTH), f64::from(HEIGHT)))).map_err(|error| anyhow!(error))?;
            platform::show_no_activate(window).map_err(|error| anyhow!(error))?;
            let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
            unsafe {
                use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE};
                SetWindowPos(hwnd, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE)?;
            }
            Ok(())
        });
        if !matches!(setup, Ok(Ok(()))) {
            let mut value = report.lock().unwrap();
            value["complete"] = json!(true);
            value["error"] = json!(format!("{setup:?}"));
            let _ = fs::write(&report_path, serde_json::to_vec_pretty(&*value).unwrap());
            cx.quit();
            return;
        }
        let checks = if scenario == "interaction" {
            line.update(cx, exercise_interaction)
        } else { Vec::new() };
        cx.spawn(async move |cx| {
            // Let the staggered drop settle before reading pixels.
            Timer::after(Duration::from_millis(1400)).await;
            let outcome = handle.update(cx, |stage, window, cx| -> anyhow::Result<Value> {
                let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
                let after = unsafe { GetForegroundWindow().0 as usize };
                let no_activate = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } & WS_EX_NOACTIVATE.0 as isize != 0;
                ensure!(after != hwnd.0 as usize && no_activate, "Fixture activated or lost no-activate");
                let line = stage.line.read(cx);
                let (tags, knots, add) = line.layout();
                Ok(json!({"hwnd": hwnd.0 as usize, "noActivate": no_activate, "phase": format!("{:?}", line.trigger.phase()),
                    "tags": tags.iter().map(|(_, tag)| json!([tag.left, tag.top, tag.width])).collect::<Vec<_>>(),
                    "knots": knots, "addLeft": add, "regions": line.regions(),
                    "sheets": line.sheet_boxes().iter().map(|(index, sheet)| json!({"tag": line.sheets[*index].tag, "pinned": line.sheets[*index].pinned,
                        "left": sheet.left, "top": sheet.top, "height": sheet.height})).collect::<Vec<_>>(),
                    "foregroundBefore": before, "foregroundAfter": after, "pointerPolled": false, "inputInjected": false,
                    "modelCommands": scenario == "interaction", "checks": checks}))
            });
            let outcome = (|| -> anyhow::Result<Value> {
                let mut detail = outcome??;
                detail["screenshot"] = capture(detail["hwnd"].as_u64().context("Missing fixture HWND")? as isize, &screenshot)?;
                Ok(detail)
            })();
            let mut value = report.lock().unwrap();
            value["complete"] = json!(true);
            match outcome {
                Ok(detail) => {
                    value["passed"] = json!(detail["checks"].as_array().is_some_and(|checks| checks.iter().all(|check| check["passed"] == true)));
                    value["detail"] = detail;
                }
                error => value["error"] = json!(format!("{error:?}")),
            }
            let _ = fs::write(&report_path, serde_json::to_vec_pretty(&*value).unwrap());
            let _ = cx.update(|cx| cx.quit());
        }).detach();
    });
    ensure!(completed.lock().unwrap()["passed"] == true, "File line fixture failed; see {}", final_path.display());
    Ok(())
}

/// The production model, frozen: no poll task, a fake monitor, fixed nodes.
fn fixture_line(host: Arc<HostState>, tokens: DesignTokens, dark: bool, scenario: &str) -> FileLine {
    let rect = Rect { left: 0.0, top: 0.0, width: f64::from(WIDTH), height: f64::from(HEIGHT) };
    let (slots, add) = line_slots(&root());
    let mut line = FileLine {
        host, tokens, dark, trigger: EdgeTrigger::default(), edge: Some(ScreenEdge { monitor: rect, work: rect, dpi_scale: 1.0 }),
        session: Some("fixture".into()), root_task: None, action_task: None, hover_task: None, slots, add, sheets: Vec::new(),
        status: None, epoch: 1, mouse_down: false, interaction: super::Interaction::default(), native_drop: super::native_drop::FileLineDrop::default(), target: 0, suspended: false, shown_edge: None, line_window: None, thread_window: None, line_shown: false, thread_shown: false,
        _poll: Task::ready(()),
    };
    let node = |id: &str| root().into_iter().find(|item| item["id"] == id).expect("fixture tag");
    let file = |id: &str, title: &str, path: &str| item(id, "Item", title, "file", "Invoke", path);
    let folder = |id: &str, title: &str, path: &str| item(id, "Folder", title, "file", "InvokeAndExpand", path);
    let project = vec![folder("clip", "剪贴板", r"D:\项目\剪贴板"), folder("site", "网站", r"D:\项目\网站"), file("index", "项目索引.md", r"D:\项目\项目索引.md")];
    let clip = folder("clip", "剪贴板", r"D:\项目\剪贴板");
    let clip_items = vec![folder("design", "设计", r"D:\项目\剪贴板\设计"), file("readme", "README.md", r"D:\项目\剪贴板\README.md"), file("needs", "需求记录.txt", r"D:\项目\剪贴板\需求记录.txt")];
    let recent = vec![file("r1", "项目索引.md", r"D:\项目\项目索引.md"), file("r2", "工作计划.docx", r"C:\Users\demo\Documents\工作计划.docx"),
        file("r3", "会议纪要.pdf", r"C:\Users\demo\Downloads\会议纪要.pdf"), file("r4", "界面参考.png", r"C:\Users\demo\Pictures\界面参考.png")];
    let pinned = Sheet { tag: "recent".into(), pinned: true, levels: vec![level(node("recent"), recent)], task: None };
    match scenario {
        "peek" => {
            let now = Instant::now();
            let sample = |now| Sample { now, x: 400, at_edge: true, in_peek_band: true, in_line: false, blocked: false };
            line.trigger.step(sample(now));
            line.trigger.step(sample(now + Duration::from_millis(220)));
        }
        "loose" => line.sheets.push(pinned),
        _ => {
            line.trigger.drop_now(true);
            line.sheets.push(pinned);
            line.sheets.push(Sheet { tag: "project".into(), pinned: false, levels: vec![level(node("project"), project), level(clip, clip_items)], task: None });
        }
    }
    if scenario == "tray" {
        let tray = line.tray_rect();
        line.interaction.gesture = Some(super::interaction::Gesture {
            node: node("project"), start: (180.0, 30.0), point: (tray.0 + tray.2 / 2.0, tray.1 + tray.3 / 2.0), active:true, removable:true,
        });
    }
    if scenario == "undo" {
        line.host.file_menu_replace(vec![r"D:\fixture-only-removed".into()]).expect("save isolated undo fixture");
        let undo = line.host.file_line_take_down(r"D:\fixture-only-removed").expect("remove isolated undo fixture").expect("removed entry");
        line.interaction.undo = Some((undo, Instant::now() + Duration::from_secs(6)));
    }
    line
}

/// Drive the same keyboard and cancellation handlers used by the native hook.
/// Clearing geometry suppresses extra native windows; no global input is sent,
/// and clearing the fake session prevents filesystem queries for sample paths.
fn exercise_interaction(line: &mut FileLine, cx: &mut gpui::Context<FileLine>) -> Vec<Value> {
    use super::interaction::{Focus, Gesture};
    let mut checks = Vec::new();
    let mut check = |name: &str, passed: bool| checks.push(json!({"name":name, "passed":passed}));
    line.edge = None;
    line.session = None;
    line.interaction.focus = Some(Focus::Tag("project".into()));
    line.handle_key("ArrowRight", false, cx);
    check("right_moves_to_next_tag", line.interaction.focus == Some(Focus::Tag("material".into())));
    line.handle_key("Home", false, cx);
    check("home_moves_to_first_tag", line.interaction.focus == Some(Focus::Tag("opened".into())));
    line.handle_key("End", false, cx);
    check("end_moves_to_last_tag", line.interaction.focus == Some(Focus::Tag("recent".into())));
    line.interaction.focus = Some(Focus::Tag("project".into()));
    line.handle_key("ArrowDown", false, cx);
    check("down_enters_existing_sheet", line.interaction.focus == Some(Focus::Sheet("project".into(), Some(0))));
    line.handle_key("ArrowDown", false, cx);
    check("down_selects_next_row", line.interaction.focus == Some(Focus::Sheet("project".into(), Some(1))));
    line.handle_key("ArrowUp", false, cx);
    check("up_selects_previous_row", line.interaction.focus == Some(Focus::Sheet("project".into(), Some(0))));
    line.handle_key("ArrowLeft", false, cx);
    check("left_returns_within_same_sheet", line.sheets.iter().find(|sheet| sheet.tag == "project").is_some_and(|sheet| sheet.levels.len() == 1));
    line.handle_key("ArrowRight", false, cx);
    check("right_drills_into_selected_folder", line.sheets.iter().find(|sheet| sheet.tag == "project")
        .is_some_and(|sheet| sheet.levels.len() == 2 && sheet.levels[1].node["id"] == "clip"));
    line.handle_key("Escape", false, cx);
    check("escape_from_child_goes_back", line.sheets.iter().find(|sheet| sheet.tag == "project").is_some_and(|sheet| sheet.levels.len() == 1));
    line.handle_key("Space", false, cx);
    check("space_pins_active_sheet", line.sheets.iter().find(|sheet| sheet.tag == "project").is_some_and(|sheet| sheet.pinned));
    line.handle_key("Escape", false, cx);
    check("escape_closes_sheet_and_returns_to_tag", !line.sheets.iter().any(|sheet| sheet.tag == "project")
        && line.interaction.focus == Some(Focus::Tag("project".into())));

    for id in ["project", "material", "tools"] {
        line.interaction.focus = Some(Focus::Tag(id.into()));
        line.handle_key("Space", false, cx);
    }
    check("fourth_pin_releases_oldest", line.sheets.iter().filter(|sheet| sheet.pinned).count() == 3
        && !line.sheets.iter().any(|sheet| sheet.tag == "recent"));
    line.handle_key("Escape", false, cx);
    check("escape_folds_but_preserves_pins", line.trigger.phase() == super::Phase::Hidden
        && line.sheets.len() == 3 && line.sheets.iter().all(|sheet| sheet.pinned) && !line.interaction.keyboard_mode);

    line.trigger.drop_now(true);
    let before = line.host.file_menu_mutation_body();
    let drag_node = root().into_iter().find(|node| node["id"] == "project").expect("fixture project");
    line.interaction.gesture = Some(Gesture { node:drag_node, start:(160.0, 30.0), point:(1010.0, 110.0), active:true, removable:true });
    line.handle_key("Escape", false, cx);
    check("escape_cancels_drag_without_mutation", line.interaction.gesture.is_none() && line.interaction.task.is_none()
        && line.action_task.is_none() && line.host.file_menu_mutation_body() == before && line.trigger.phase() == super::Phase::Down);
    check("cancel_suppresses_following_click", line.interaction.suppress_click_until.is_some_and(|until| until > Instant::now()));

    line.host.file_menu_replace(vec!["fixture-only-expiry".into()]).expect("save private expiry fixture");
    let undo = line.host.file_line_take_down("fixture-only-expiry").expect("remove private fixture").expect("undo token");
    line.interaction.undo = Some((undo, Instant::now() - Duration::from_millis(1)));
    line.expire_undo(cx);
    check("expired_undo_disappears", line.interaction.undo.is_none());

    // Leave a useful focus-ring capture after the assertions have exercised
    // folding and cancellation. The stage is still the only visible HWND.
    *line = fixture_line(line.host.clone(), line.tokens.clone(), line.dark, "down");
    line.interaction.keyboard_mode = true;
    line.interaction.focus = Some(Focus::Sheet("project".into(), Some(1)));
    cx.notify();
    checks
}

/// A desktop-like backdrop under the transparent thread and overlay.
struct Stage {
    line: Entity<FileLine>,
    thread: Entity<ThreadView>,
    overlay: Entity<LineView>,
}

impl Render for Stage {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let line = self.line.read(cx);
        let t = line.tokens.theme(line.dark);
        // Same visibility rule as `sync_windows`: the thread only peeks or holds clipped sheets.
        let phase = line.trigger.phase();
        let thread = phase == super::Phase::Peek || phase == super::Phase::Hidden && line.pinned();
        div().relative().size_full().bg(color(&t.secondary))
            .child(div().absolute().left(px(240.0)).top(px(56.0)).w(px(700.0)).h(px(380.0)).rounded(px(9.0)).border_1()
                .border_color(color(&t.border)).bg(color(&t.background)))
            .when(thread, |stage| stage.child(div().absolute().left_0().top_0().w(px(WIDTH)).h(px(16.0)).child(self.thread.clone())))
            .child(div().absolute().left_0().top_0().size_full().child(self.overlay.clone()))
    }
}
