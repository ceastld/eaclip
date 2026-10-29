//! Isolated production-popup rendering. Fixed DTOs, no capture, hooks, API,
//! singleton, tray, LAN or global input. All profile/cache files stay in .tmp.

use super::{ChoiceMode, Choices, Column, FavoriteSavePopup, FinishHandler, Layout};
use crate::{
    platform,
    preview::{DesignTokens, TOKENS_JSON},
    DesignAssets,
};
use anyhow::{anyhow, ensure, Context as _};
use cliphost_core::{window_place::overlay_work_area_from_point, AppPaths, HostState};
use gpui::{div, prelude::*, Application, Context, Timer, Window, WindowHandle, WindowOptions};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path as FsPath, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
};

struct Owner;
impl Render for Owner {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn sample() -> Value {
    let folders = [
        ("work", "工作"),
        ("personal", "个人"),
        ("reference", "参考资料"),
        ("inbox", "稍后整理"),
        ("projects", "项目"),
        ("work-copy", "常用文本"),
        ("work-links", "网址"),
        ("clip", "剪贴板"),
        ("site", "产品网站"),
        ("design", "界面设计"),
        ("research", "调研记录"),
        ("personal-copy", "常用文本"),
        ("travel", "旅行"),
        ("personal-links", "网址"),
        ("typography", "字体与排版"),
        ("reference-design", "界面设计"),
        ("components", "组件参考"),
    ];
    json!({"dragRevision":"fixture-only-no-persistence", "orderedRootItems":["work","personal","reference","inbox"],
        "items":folders.into_iter().map(|(id,title)|(id.to_owned(),json!({"id":id,"title":title,"isFolder":true}))).collect::<serde_json::Map<_,_>>(),
        "childrenById":{"work":["projects","work-copy","work-links"],"projects":["clip","site"],"clip":["design","research"],
            "personal":["personal-copy","travel","personal-links"],"reference":["typography","reference-design"],"reference-design":["components"]},
        "recentFolders":[{"id":"design","path":["work","projects","clip","design"]},
            {"id":"work-copy","path":["work","work-copy"]},{"id":"personal-copy","path":["personal","personal-copy"]}]})
}

pub fn run(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<()> {
    let (mut output, mut theme, mut scenario, mut exit_after) =
        (None, String::from("dark"), String::from("root"), 5_000u64);
    let mut args = arguments.into_iter();
    while let Some(key) = args.next() {
        if matches!(key.as_str(), "-h" | "--help") {
            println!("favorite_save_visual --output FRESH_ABSOLUTE_REPO_TMP_DIRECTORY [--theme dark|light] [--scenario root|cascade|search|narrow] [--exit-after-ms 1000..60000]");
            return Ok(());
        }
        let value = args
            .next()
            .with_context(|| format!("{key} requires a value"))?;
        match key.as_str() {
            "--output" => output = Some(PathBuf::from(value)),
            "--theme" => {
                ensure!(["dark", "light"].contains(&value.as_str()), "Invalid theme");
                theme = value;
            }
            "--scenario" => {
                ensure!(
                    ["root", "destination", "cascade", "search", "narrow"]
                        .contains(&value.as_str()),
                    "Invalid scenario"
                );
                scenario = value;
            }
            "--exit-after-ms" => {
                exit_after = value.parse()?;
                ensure!(
                    (1000..=60_000).contains(&exit_after),
                    "Invalid exit duration"
                );
            }
            _ => return Err(anyhow!("Unknown argument: {key}")),
        }
    }
    let output = output.context("--output is required")?;
    ensure!(output.is_absolute(), "--output must be absolute");
    let repository = FsPath::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .context("Missing repository root")?
        .canonicalize()?;
    let scratch = repository
        .join(".tmp")
        .canonicalize()
        .context("Create repository .tmp first")?;
    ensure!(
        scratch.parent() == Some(repository.as_path()),
        "Repository .tmp must not be a junction outside the repository"
    );
    let parent = output
        .parent()
        .context("Output needs a parent")?
        .canonicalize()?;
    ensure!(
        parent.starts_with(&scratch),
        "--output must be inside repository .tmp"
    );
    let run = parent.join(output.file_name().context("Output needs a name")?);
    fs::create_dir(&run).context("--output must be a fresh nonexistent directory")?;
    let run = run.canonicalize()?;
    ensure!(run.starts_with(&scratch), "Output escaped repository .tmp");
    let data = run.join("data");
    fs::create_dir(&data)?;
    let profile = run.join("profile");
    fs::create_dir(&profile)?;
    for name in ["Local", "Roaming"] {
        fs::create_dir(profile.join(name))?;
    }
    std::env::set_var("APPDATA", profile.join("Roaming"));
    std::env::set_var("LOCALAPPDATA", profile.join("Local"));
    std::env::set_var("BETTERCLIP_TAURI_DATA_ROOT", &data);
    cliphost_core::force_headless(true);
    let host = HostState::open(AppPaths::from_data_root(data.clone()), 29065)
        .map_err(|error| anyhow!(error))?;
    ensure!(
        host.lan_status()["enabled"] == false && host.lan_listener_port().is_none(),
        "Fresh core enabled LAN"
    );
    let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
    let report_path = run.join("favorite-save-report.json");
    fs::write(
        &report_path,
        serde_json::to_vec_pretty(
            &json!({"pid":std::process::id(),"complete":false,"scenario":scenario,"dataRoot":data}),
        )?,
    )?;
    Application::new().with_assets(DesignAssets).run(move |cx| {
        let before = unsafe { GetForegroundWindow().0 as usize };
        let setup = (|| -> anyhow::Result<WindowHandle<FavoriteSavePopup>> {
            let owner = cx.open_window(WindowOptions { titlebar: None, focus: false, show: false,
                is_resizable: false, is_minimizable: false, ..Default::default() }, |window, cx| {
                    window.set_window_title("ClipHost · isolated favorite save owner"); cx.new(|_| Owner)
                })?;
            let owner_hwnd = owner.update(cx, |_, window, _| platform::hwnd(window).map(|hwnd| hwnd.0 as isize).map_err(|error| anyhow!(error)))??;
            host.set_host_hwnd(owner_hwnd); host.request_show();
            let lease = crate::menu::next_lease(); ensure!(host.acquire_native_context_menu(lease, "clipboard"), "Fixture lease was refused");
            let area = overlay_work_area_from_point(1000, 250).context("No fixture monitor")?;
            let scale = area.dpi_scale_y;
            let actual = (area.work.left.round() as i32, area.work.top.round() as i32,
                area.work.width.round() as i32, area.work.height.round() as i32);
            let dip = |value: f64| (value * scale).round() as i32;
            let (anchor, work) = if scenario == "narrow" {
                let width = dip(320.0).min(actual.2);
                let height = dip(400.0).min(actual.3);
                let left = actual.0 + (actual.2 - width) / 2;
                let top = actual.1 + (actual.3 - height) / 2;
                ((left + dip(100.0), top + dip(60.0)), (left, top, width, height))
            } else {
                ((actual.0 + actual.2 - dip(300.0), actual.1 + dip(150.0)), actual)
            };
            let layout = Layout::place(anchor, work, scale);
            let finish_host = host.clone();
            let finish: FinishHandler = Rc::new(move |lease, _, cx| { finish_host.release_native_context_menu(lease); cx.quit(); });
            let popup = cx.open_window(WindowOptions { titlebar: None, focus: false, show: false,
                is_resizable: false, is_minimizable: false,
                window_background: gpui::WindowBackgroundAppearance::Transparent,
                ..Default::default() }, |window, cx| {
                    window.set_window_title("ClipHost · favorite save isolated fixture");
                    cx.new(|cx| FavoriteSavePopup::new(host.clone(), tokens, theme == "dark", layout, finish, window, cx))
                })?;
            popup.update(cx, |view, window, cx| -> anyhow::Result<()> {
                view.lease = lease; view.ids = vec!["fixture-note".into(),"fixture-link".into(),"fixture-files".into()];
                view.choices = Choices::parse(&sample()).map_err(|error| anyhow!(error))?;
                view.columns = vec![Column { parent: Vec::new(), top: 8.0 }];
                if scenario == "cascade" || scenario == "destination" {
                    let parents = [vec!["work".into()], vec!["work".into(), "projects".into()]];
                    for (index, parent) in parents.into_iter().take(layout.columns - 1).enumerate() {
                        view.columns.push(Column { parent, top: 230.0 + index as f32 * 20.0 });
                    }
                    let index = view.columns.len() - 1;
                    let mut path = view.columns[index].parent.clone();
                    path.push(if index == 2 { "clip" } else if index == 1 { "projects" } else { "work" }.into());
                    view.hovered = Some((index, ChoiceMode::Browse, path));
                } else if scenario == "narrow" {
                    view.columns[0].parent = vec!["work".into(), "projects".into(), "clip".into()];
                    view.hovered = Some((0, ChoiceMode::Browse, vec!["work".into(), "projects".into(), "clip".into(), "design".into()]));
                } else if scenario == "search" {
                    view.query = "界面设计".into();
                    view.search.update(cx, |search, cx| search.set_text("界面设计".into(), window, cx));
                }
                view.configure_input(cx); window.focus(&view.focus);
                let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
                ensure!(view.host.set_native_context_menu_hwnd(lease, hwnd.0 as isize), "Fixture HWND registration failed");
                platform::set_geometry(window, Some((layout.rect.0, layout.rect.1)), None).map_err(|error| anyhow!(error))?;
                platform::overlay_set_regions(window, vec![(layout.root_x - 6.0, 2.0, layout.root_width + 12.0, layout.height + 12.0)]).map_err(|error| anyhow!(error))?;
                platform::overlay_show(window, owner_hwnd, layout.rect).map_err(|error| anyhow!(error))?;
                cx.notify(); Ok(())
            })??;
            Ok(popup)
        })();
        let popup = match setup { Ok(popup) => popup, Err(error) => {
            let _ = fs::write(&report_path, serde_json::to_vec_pretty(&json!({"complete":true,"error":format!("{error:#}")})).unwrap());
            cx.quit(); return;
        }};
        cx.spawn(async move |cx| {
            let started = Instant::now(); let mut ever_foreground = false;
            loop {
                Timer::after(Duration::from_millis(200)).await;
                let report = popup.update(cx, |view, window, _| -> anyhow::Result<Value> {
                    let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
                    ever_foreground |= unsafe { GetForegroundWindow() } == hwnd;
                    let rectangles: Vec<_> = view.card_regions.borrow().iter().map(|(index, rect)| json!({"column":index,"rectDip":rect})).collect();
                    Ok(json!({"pid":std::process::id(),"popupHwnd":hwnd.0 as usize,"ownerHwnd":view.host.host_hwnd(),
                        "title":"ClipHost · favorite save isolated fixture","theme":if view.dark {"dark"} else {"light"},
                        "scenario":scenario,"rectPx":view.layout.rect,"cards":rectangles,"sourceCount":view.ids.len(),
                        "folderOccurrences":view.choices.folders.len(),"columns":view.columns.len(),"dataRoot":data,
                        "columnParents":view.columns.iter().map(|column| &column.parent).collect::<Vec<_>>(),
                        "destinationLabel":view.hovered.as_ref().map(|key| view.choices.label(&key.2)),
                        "hasBreadcrumbs":!view.columns[0].parent.is_empty(),"hasSubmit":false,
                        "noActivate":unsafe { GetWindowLongPtrW(hwnd,GWL_EXSTYLE) } & WS_EX_NOACTIVATE.0 as isize != 0,
                        "foregroundBefore":before,"foregroundAfter":unsafe { GetForegroundWindow().0 as usize },
                        "everForeground":ever_foreground,"complete":started.elapsed() >= Duration::from_millis(exit_after),
                        "elapsedMs":started.elapsed().as_millis(),"globalInputInjected":false,"hooksStarted":false,
                        "clipboardCaptureStarted":false,"networkListenerStarted":false}))
                });
                match report {
                    Ok(Ok(report)) => {
                        let _ = fs::write(&report_path, serde_json::to_vec_pretty(&report).unwrap());
                        if report["complete"] == true || report["everForeground"] == true {
                            let _ = cx.update(|cx| cx.quit()); break;
                        }
                    }
                    _ => { let _ = cx.update(|cx| cx.quit()); break; }
                }
            }
        }).detach();
    });
    Ok(())
}
