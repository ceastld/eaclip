//! Fixed mixed rows through SuperMenuWindow; disposable headless core only.

use super::{
    Category, Column, SessionKey,
    SuperMenuWindow,
};
use crate::{
    platform,
    preview::{DesignTokens, TOKENS_JSON},
    DesignAssets,
};
use anyhow::{anyhow, ensure, Context as _};
use cliphost_core::{AppPaths, HostState};
use gpui::{prelude::*, px, size, Application, Bounds, Timer, WindowBounds, WindowOptions};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
};

pub(crate) fn output_directory(output: &Path) -> anyhow::Result<PathBuf> {
    ensure!(output.is_absolute(), "--output must be absolute");
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .context("Missing repository root")?
        .canonicalize()?;
    let scratch = repository.join(".tmp").canonicalize()?;
    ensure!(
        scratch.parent() == Some(repository.as_path()),
        "Repository .tmp escaped repository"
    );
    let parent = output
        .parent()
        .context("Missing output parent")?
        .canonicalize()?;
    ensure!(
        parent.starts_with(&scratch),
        "Output must stay inside repository .tmp"
    );
    let run = parent.join(output.file_name().context("Missing output name")?);
    fs::create_dir(&run).context("--output must be a fresh nonexistent directory")?;
    let run = run.canonicalize()?;
    ensure!(run.starts_with(scratch), "Output escaped repository .tmp");
    for name in ["data", "profile", "profile/Local", "profile/Roaming"] {
        fs::create_dir(run.join(name))?;
    }
    std::env::set_var("APPDATA", run.join("profile/Roaming"));
    std::env::set_var("LOCALAPPDATA", run.join("profile/Local"));
    std::env::set_var("BETTERCLIP_TAURI_DATA_ROOT", run.join("data"));
    Ok(run)
}

fn sample() -> Value {
    let mut items: Vec<_> = [
        ("添加目录…", false),
        ("添加已打开的文件夹…", true),
        ("Documents and Settings", false),
        ("项目与文档", true),
        ("FufuToolbox", false),
        ("Program Files (x86)", true),
        ("Temp", false),
        ("最近使用的文件", true),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (title, children))| {
        json!({
            "id":format!("style-{index}"), "kind":"Command", "title":title,
            "isEnabled":true, "hasChildren":children, "hasContextMenu":false,
            "invokeMode":if children {"Expand"} else {"None"},
        })
    })
    .collect();
    items.insert(2, json!({"id":"style-separator", "kind":"Separator"}));
    json!({"status":"Ready", "items":items})
}

/// Same node shape as `menu_api::window_node` / `window_ops` / `window_context_from_info`.
fn window_samples() -> (Value, Value, Value) {
    let window = |id: &str, title: &str, app: &str, exe: &str, icon: &str, state: Value| {
        let mut visual = json!({"title":title, "app":app, "exe":exe, "pid":4208, "foreground":false,
            "topmost":false, "minimized":false, "maximized":false, "opacity":100, "penetrate":false});
        for (key, value) in state.as_object().into_iter().flatten() {
            visual[key] = value.clone();
        }
        json!({"id":id, "kind":"Folder", "role":"window", "title":title, "description":title, "icon":icon,
            "isEnabled":true, "hasChildren":true, "hasContextMenu":true, "invokeMode":"InvokeAndExpand",
            "payload":{"windowHandle":4208}, "visual":{"window":visual}})
    };
    let root = json!({"status":"Ready", "items":[
        window("fixture-window-code", "super_menu.rs — clip — Visual Studio Code", "Visual Studio Code", "Code.exe",
            "fa:Light_Code", json!({"foreground":true})),
        window("fixture-window-browser", "GPUI 文档 · Window - Microsoft Edge", "Microsoft Edge", "msedge.exe",
            "fa:Light_Globe", json!({"maximized":true})),
        window("fixture-window-notes", "DESIGN.md — clip — Visual Studio Code", "Visual Studio Code", "Code.exe",
            "fa:Light_Code", json!({"topmost":true, "opacity":85})),
        {"id":"fixture-window-group", "kind":"Folder", "role":"window-group", "title":"【前台】explorer clip",
            "description":"C:\\example\\project\\clip", "icon":"icon:C:\\Windows\\explorer.exe",
            "isEnabled":true, "hasChildren":true, "hasContextMenu":true, "invokeMode":"InvokeAndExpand",
            "payload":{"windowHandle":5680},
            "visual":{"windowGroup":{"app":"Windows 资源管理器", "count":3, "recent":"clip", "foreground":false}}},
        window("fixture-window-photos", "界面草图.png - 照片", "Microsoft Photos", "Photos.exe",
            "fa:Light_Image", json!({"opacity":75, "penetrate":true})),
        window("fixture-window-notepad", "待办事项.txt - 记事本", "Notepad", "Notepad.exe",
            "icon:C:\\Windows\\System32\\notepad.exe", json!({"minimized":true})),
        {"id":"fixture-separator", "kind":"Separator"},
        {"id":"fixture-setting", "kind":"Command", "role":"setting", "title":"设置", "icon":"fa:Light_Cog", "isEnabled":true}
    ]});
    let fields = [
        ("标题", "super_menu.rs — clip — Visual Studio Code"),
        ("句柄", "4208"),
        ("类名", "Chrome_WidgetWin_1"),
        ("进程", "Code"),
        ("PID", "4208"),
        ("路径", "C:\\Program Files\\Microsoft VS Code\\Code.exe"),
        ("位置", "144, 96 · 1480 × 920"),
    ];
    let mut info: Vec<Value> = fields.into_iter().enumerate().map(|(index, (label, value))|
        json!({"id":format!("fixture-copy-{index}"), "kind":"Command", "role":"window-copy", "icon":"fa:Light_Copy",
            "title":format!("复制{label}:  {value}"), "description":value, "payload":{"text":value}, "isEnabled":true,
            "visual":{"field":{"label":label, "value":value}}})
    ).collect();
    info.extend([
        json!({"id":"fixture-copy-all", "kind":"Command", "role":"window-copy", "icon":"fa:Light_Copy", "title":"复制全部信息", "isEnabled":true}),
        json!({"id":"fixture-info-separator", "kind":"Separator"}),
        json!({"id":"fixture-locate", "kind":"Command", "role":"window-locate", "icon":"fa:Light_FolderOpen", "title":"打开进程所在文件夹", "isEnabled":true}),
        json!({"id":"fixture-close-class", "kind":"Command", "role":"window-close-class", "icon":"fa:Light_Times", "title":"关闭所有 Chrome_WidgetWin_1 窗口", "isEnabled":true}),
        json!({"id":"fixture-info-separator-2", "kind":"Separator"}),
        json!({"id":"fixture-kill-all", "kind":"Command", "role":"window-kill-all", "icon":"fa:Light_TimesCircle:red", "title":"结束所有 Code 进程", "isEnabled":true}),
    ]);
    let context = json!({"status":"Ready", "items":info});
    let operations = json!({"status":"Ready", "items":[
        {"id":"fixture-pin", "kind":"Check", "role":"window-topmost", "title":"置顶", "icon":"fa:Light_Thumbtack", "isEnabled":true, "check":{"isChecked":true}},
        {"id":"fixture-penetrate", "kind":"Check", "role":"window-penetrate", "title":"鼠标穿透", "icon":"fa:Light_MousePointer", "isEnabled":true, "check":{"isChecked":false}},
        {"id":"fixture-ops-separator", "kind":"Separator"},
        {"id":"fixture-min", "kind":"Check", "role":"window-min", "title":"最小化", "icon":"fa:Light_WindowMinimize", "isEnabled":true, "check":{"isChecked":false}},
        {"id":"fixture-max", "kind":"Check", "role":"window-max", "title":"最大化", "icon":"fa:Light_WindowMaximize", "isEnabled":true, "check":{"isChecked":false}},
        {"id":"fixture-center", "kind":"Command", "role":"window-center", "title":"居中", "icon":"fa:Regular_SquareFull", "description":"移动到当前窗口中心", "isEnabled":true},
        {"id":"fixture-opacity", "kind":"Value", "role":"window-opacity", "title":"不透明度", "icon":"fa:Light_Adjust", "isEnabled":true, "value":{"min":26,"max":255,"step":1,"current":217}},
        {"id":"fixture-ops-separator-2", "kind":"Separator"},
        {"id":"fixture-close", "kind":"Command", "role":"window-close", "title":"关闭窗口", "icon":"fa:Light_Times", "isEnabled":true},
        {"id":"fixture-kill", "kind":"Command", "role":"window-kill", "title":"结束进程", "icon":"fa:Light_TimesCircle:red", "description":"PID 4208", "isEnabled":true}
    ]});
    (root, context, operations)
}

pub(crate) fn capture(hwnd_value: isize, path: &Path) -> anyhow::Result<Value> {
    use windows::Win32::{
        Foundation::{HWND, RECT},
        Graphics::Gdi::*,
        UI::WindowsAndMessaging::{GetClientRect, GetWindowThreadProcessId},
    };
    #[link(name = "user32")]
    extern "system" {
        #[link_name = "PrintWindow"]
        fn print_window(hwnd: HWND, dc: HDC, flags: u32) -> i32;
    }
    let hwnd = HWND(hwnd_value as *mut std::ffi::c_void);
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        ensure!(
            pid == std::process::id(),
            "Capture only this fixture's HWND"
        );
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect)?;
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        ensure!(
            (1..=4096).contains(&width) && (1..=4096).contains(&height),
            "Invalid capture dimensions"
        );
        let source = GetDC(Some(hwnd));
        ensure!(!source.is_invalid(), "GetDC failed");
        let dc = CreateCompatibleDC(Some(source));
        let bitmap = CreateCompatibleBitmap(source, width, height);
        if dc.is_invalid() || bitmap.is_invalid() {
            if !dc.is_invalid() {
                let _ = DeleteDC(dc);
            }
            if !bitmap.is_invalid() {
                let _ = DeleteObject(bitmap.into());
            }
            ReleaseDC(Some(hwnd), source);
            return Err(anyhow!("Cannot allocate capture bitmap"));
        }
        let previous = SelectObject(dc, bitmap.into());
        let printed = print_window(hwnd, dc, 3) != 0;
        SelectObject(dc, previous);
        let mut pixels = vec![0; width as usize * height as usize * 4];
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let rows = if printed {
            GetDIBits(
                dc,
                bitmap,
                0,
                height as u32,
                Some(pixels.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            )
        } else {
            0
        };
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        ReleaseDC(Some(hwnd), source);
        ensure!(printed && rows == height, "PrintWindow/GetDIBits failed");
        let first = [pixels[0], pixels[1], pixels[2]];
        let nonuniform = pixels.chunks_exact(4).any(|pixel| pixel[..3] != first);
        ensure!(nonuniform, "Capture contained no rendered detail");
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        image::save_buffer(
            path,
            &pixels,
            width as u32,
            height as u32,
            image::ColorType::Rgba8,
        )?;
        Ok(
            json!({"method":"PrintWindow(PW_CLIENTONLY|PW_RENDERFULLCONTENT)",
            "path":path,"width":width,"height":height,"nonuniform":nonuniform,"desktopRead":false}),
        )
    }
}

pub fn run(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<()> {
    let (mut output, mut dark) = (None, true);
    let mut scenario = "file".to_string();
    let mut args = arguments.into_iter();
    while let Some(key) = args.next() {
        if matches!(key.as_str(), "--help" | "-h") {
            println!("super_menu_style --output FRESH_ABSOLUTE_REPO_TMP_DIRECTORY [--theme dark|light] [--scenario file|favor|bookmarks|text|clipboard|window-root|window-info|window-management]\nFixed rows; own-HWND PNG; no input or native services.");
            return Ok(());
        }
        let value = args
            .next()
            .with_context(|| format!("{key} requires a value"))?;
        match key.as_str() {
            "--output" => output = Some(PathBuf::from(value)),
            "--scenario" => {
                ensure!([
                    "file", "favor", "bookmarks", "text", "clipboard",
                    "window-root", "window-info", "window-management", "window-thumbnail", "window-kill-confirm",
                    "window-settings", "file-settings", "window-general-settings",
                ].contains(&value.as_str()), "Invalid scenario");
                scenario = value;
            }
            "--theme" => {
                ensure!(["dark", "light"].contains(&value.as_str()), "Invalid theme");
                dark = value == "dark";
            }
            _ => return Err(anyhow!("Unknown argument: {key}")),
        }
    }
    let run = output_directory(&output.context("--output is required")?)?;
    let report_path = run.join("style-report.json");
    let screenshot = run.join("super-menu-style.png");
    cliphost_core::force_headless(true);
    let host = Arc::new(
        HostState::open(AppPaths::from_data_root(run.join("data")), 29065)
            .map_err(|error| anyhow!(error))?,
    );
    let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
    let before = unsafe { GetForegroundWindow().0 as usize };
    let report = Arc::new(Mutex::new(json!({"complete":false,"passed":false,
        "pid":std::process::id(),"theme":if dark {"dark"} else {"light"},"scenario":scenario,"dataRoot":run.join("data")})));
    let completed = report.clone();
    let final_path = report_path.clone();
    fs::write(
        &report_path,
        serde_json::to_vec_pretty(&*report.lock().unwrap())?,
    )?;
    // Window rows carry real app and command icons (2026-10-23 design).
    let window_icons = scenario.starts_with("window-");
    Application::new().with_assets(DesignAssets).run(move |cx| {
        let bounds = Bounds::centered(None, size(px(680.), px(540.)), cx);
        let view_host = host.clone();
        let handle = cx.open_window(WindowOptions { titlebar:None, focus:false, show:false,
            is_resizable:false, is_minimizable:false, window_bounds:Some(WindowBounds::Windowed(bounds)),
            window_background:gpui::WindowBackgroundAppearance::Opaque, ..Default::default()
        }, move |_, cx| cx.new(|cx|SuperMenuWindow::new(view_host, tokens, dark, cx)))
            .expect("Open isolated menu fixture");
        let setup = handle.update(cx, |menu, window, cx| -> anyhow::Result<()> {
            platform::set_no_activate(window, true).map_err(|error|anyhow!(error))?;
            platform::set_geometry(window, None, Some((680., 540.))).map_err(|error|anyhow!(error))?;
            let hwnd = platform::hwnd(window).map_err(|error|anyhow!(error))?;
            // Text session uses only fresh settings and our explicit target;
            // replace every displayed node before showing the fixture.
            menu.host.menu_create_session_ex("text", "", hwnd.0 as i64, hwnd.0 as i64);
            menu.retire(cx);
            menu.state = menu.host.super_menu_state_detail();
            menu.session = SessionKey::from_state(&menu.state);
            menu.category = match scenario.as_str() {
                "favor" => Category::Favor,
                "bookmarks" => Category::Bookmarks,
                "text" => Category::Text,
                "clipboard" => Category::Clipboard,
                _ => Category::File,
            };
            menu.source_hwnd = hwnd.0 as i64;
            menu.state["style"]["menuMaxWidth"] = json!(240.);
            menu.state["style"]["menuFontSize"] = json!(12.);
            menu.state["hostDpiScaleX"] = json!(1.);
            menu.state["hostDpiScaleY"] = json!(1.);
            menu.state["overlayLeftPx"] = json!(0.);
            menu.state["overlayTopPx"] = json!(0.);
            menu.state["anchorScreenX"] = json!(24.);
            menu.state["anchorScreenY"] = json!(240.);
            if scenario == "file-settings" {
                menu.category = Category::File;
                menu.columns.push(Column::from_page("style-root".into(), "children", 240., &sample()));
                menu.open_settings();
            }
            if scenario == "file" {
                menu.columns.push(Column::from_page("style-root".into(), "children", 240., &sample()));
            }
            if scenario.starts_with("window-") {
                let (root, info, management) = window_samples();
                menu.category = Category::Window;
                menu.state["style"]["menuMaxWidth"] = json!(300.);
                menu.state["style"]["menuFontSize"] = json!(12.);
                menu.state["style"]["windowThumbnail"] = json!(scenario == "window-thumbnail");
                menu.state["anchorScreenY"] = json!(126.);
                menu.columns = vec![Column::from_page("".into(), "children", 126., &root)];
                if scenario != "window-root" {
                    let (collection, page) = if scenario == "window-info" {
                        ("context-menu", info)
                    } else {
                        ("children", management)
                    };
                    menu.columns.push(Column::from_page("fixture-window-code".into(), collection, 126., &page));
                }
                if scenario == "window-settings" || scenario == "window-general-settings" {
                    menu.columns.truncate(1);
                    menu.open_settings();
                    menu.settings_general = scenario == "window-general-settings";
                }
                if scenario == "window-kill-confirm" {
                    let kill = menu.columns[1].items.last().cloned().expect("kill row");
                    let parent = menu.columns[0].items.first().cloned();
                    menu.kill_confirm = Some(super::kill_confirm_for(&kill, parent.as_ref()));
                }
            }
            ensure!(menu.render_current(&menu.session, menu.value_presentation), "Fixture session invalid");
            cx.notify();
            platform::show_no_activate(window).map_err(|error|anyhow!(error))?;
            unsafe {
                use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE};
                SetWindowPos(hwnd, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOACTIVATE|SWP_NOMOVE|SWP_NOSIZE)?;
            }
            Ok(())
        });
        if !matches!(setup, Ok(Ok(()))) {
            let mut value = report.lock().unwrap();
            value["complete"] = json!(true); value["error"] = json!(format!("{setup:?}"));
            let _ = fs::write(&report_path, serde_json::to_vec_pretty(&*value).unwrap());
            cx.quit(); return;
        }
        cx.spawn(async move |cx| {
            // Allow layout, actual painting, and the production column entrance
            // animation to settle without touching the user's physical pointer.
            Timer::after(Duration::from_millis(1000)).await;
            let outcome = handle.update(cx, |menu, window, _| -> anyhow::Result<Value> {
                let hwnd = platform::hwnd(window).map_err(|error|anyhow!(error))?;
                let after = unsafe {GetForegroundWindow().0 as usize};
                let no_activate = unsafe {GetWindowLongPtrW(hwnd,GWL_EXSTYLE)} & WS_EX_NOACTIVATE.0 as isize != 0;
                ensure!(after != hwnd.0 as usize && no_activate, "Fixture activated or lost no-activate");
                ensure!(menu.keyboard_request.is_none() && menu.native_key_request.is_none()
                    && menu.column_tasks.is_empty() && menu.action_task.is_none()
                    && (window_icons || menu.node_icons.is_empty()), "Unexpected menu host work");
                ensure!(cliphost_core::is_headless() && host.lan_status()["enabled"] == false
                    && host.lan_listener_port().is_none(), "Core isolation failed");
                let viewport = (f32::from(window.viewport_size().width), f32::from(window.viewport_size().height));
                let category_bar = menu.strip_rect(viewport);
                Ok(json!({"hwnd":hwnd.0 as usize,"noActivate":no_activate,
                    "category":menu.category.action(),
                    "categoryRectDip":{"x":category_bar.x,"y":category_bar.y,
                        "width":category_bar.width,"height":category_bar.height},
                    "viewportDip":[viewport.0,viewport.1],
                    "scaleFactor":window.scale_factor(),"columns":menu.columns.iter().map(|column|&column.items).collect::<Vec<_>>(),
                    "foregroundBefore":before,"foregroundAfter":after,"foregroundPreserved":after == before,
                    "headless":true,"apiStarted":false,"rpcStarted":false,"nativeHostStarted":false,
                    "captureStarted":false,"hooksStarted":false,"lanStarted":false,"inputInjected":false,
                    "boundsMeasurement":"row bounds not instrumented; inspect actual rendered PNG"}))
            });
            // PrintWindow may enter the native dispatcher; release the GPUI
            // entity/window update borrow before requesting its own pixels.
            let outcome = (|| -> anyhow::Result<Value> {
                let mut detail = outcome??;
                detail["screenshot"] = capture(detail["hwnd"].as_u64().context("Missing fixture HWND")? as isize, &screenshot)?;
                let after_capture = unsafe {GetForegroundWindow().0 as usize};
                ensure!(Some(after_capture as u64) != detail["hwnd"].as_u64(), "Fixture became foreground during capture");
                detail["foregroundAfterCapture"] = json!(after_capture);
                Ok(detail)
            })();
            let mut value = report.lock().unwrap();
            value["complete"] = json!(true);
            match outcome {
                Ok(detail) => { value["passed"] = json!(true); value["detail"] = detail; }
                error => value["error"] = json!(format!("{error:?}")),
            }
            let _ = fs::write(&report_path, serde_json::to_vec_pretty(&*value).unwrap());
            let _ = cx.update(|cx|cx.quit());
        }).detach();
    });
    ensure!(
        completed.lock().unwrap()["passed"] == true,
        "Style fixture failed; see {}",
        final_path.display()
    );
    println!("Menu style capture passed: {}", final_path.display());
    Ok(())
}
