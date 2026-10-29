//! Isolated production history-menu rendering. Fixed DTOs, no clipboard,
//! hooks, listeners, transport actions or injected input; files stay in .tmp.

use super::{
    ContextMenuPopup, Groups, HistoryMenuContext, HistoryMenuPeer, HistorySelectHandler, Layout,
    MenuAction, MenuEntry,
};
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
    ffi::c_void,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Gdi::{BITMAPINFO, BITMAPINFOHEADER},
    UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GWL_EXSTYLE, WS_EX_NOACTIVATE,
    },
};

struct Owner;
impl Render for Owner {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn sample(scenario: &str) -> (Vec<MenuEntry>, HistoryMenuContext, Vec<HistoryMenuPeer>) {
    let batch = matches!(scenario, "batch" | "devices");
    let files = scenario == "files";
    let mut entries = vec![
        MenuAction::tile(
            "paste",
            if batch {
                "按选择顺序粘贴"
            } else {
                "粘贴"
            },
        ),
        MenuAction::tile("copy-only", "复制"),
        MenuAction::tile("favorite", "收藏"),
        MenuAction::tile("pin-toggle", if batch { "切换置顶" } else { "置顶" }),
        MenuAction::danger_tile("delete", "删除"),
    ];
    if !files {
        entries.push(MenuAction::tile("paste-plain-text", "纯文本粘贴"));
    }
    if batch {
        entries.push(MenuAction::tile("reverse-paste", "逆序粘贴"));
    }
    entries.push(MenuAction::tile("fifo-paste", "连续粘贴…"));
    if files {
        entries.push(MenuAction::tile("reveal-file", "定位文件"));
        entries.push(MenuAction::tile("reveal-folder", "定位文件夹"));
    }
    if !batch {
        entries.push(MenuAction::row("edit", "编辑内容"));
    }
    entries.push(MenuAction::row("lan-share", "发送到局域网"));
    if files {
        entries.push(MenuAction::row("split-files", "按文件拆分"));
    } else if scenario == "left" {
        entries.push(MenuAction::row("split-lines", "按行拆分"));
    }
    entries.push(MenuAction::row("distinct", "去除重复"));
    let context = HistoryMenuContext {
        title: if batch {
            "选择顺序"
        } else if files {
            "界面设计稿.fig 等 3 个文件"
        } else {
            "整理右键菜单，让常用操作更容易找到"
        }
        .into(),
        kind_label: if batch {
            "已选 3 项"
        } else if files {
            "文件"
        } else {
            "文本"
        }
        .into(),
        icon: if batch {
            "list"
        } else if files {
            "file"
        } else {
            "text"
        }
        .into(),
        count: if batch { 3 } else { 1 },
    };
    let peers = [
        ("fixture-desktop", "办公室电脑", "desktop", true),
        ("fixture-laptop", "我的笔记本", "laptop", true),
        ("fixture-phone", "手机", "phone", false),
    ]
    .into_iter()
    .map(|(id, name, icon, online)| HistoryMenuPeer {
        id: id.into(),
        name: name.into(),
        icon: icon.into(),
        online,
        paired: true,
    })
    .collect();
    (entries, context, peers)
}

fn write_report(path: &Path, report: &Value) -> anyhow::Result<()> {
    // Publish only a complete JSON document; readers may retry the short
    // replacement gap while the popup is still rendering.
    let pending = path.with_extension("pending.json");
    fs::write(&pending, serde_json::to_vec_pretty(report)?)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(pending, path)?;
    Ok(())
}

pub fn run(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<()> {
    let (mut output, mut theme, mut scenario, mut exit_after) =
        (None, String::from("dark"), String::from("text"), 5_000u64);
    let mut args = arguments.into_iter();
    while let Some(key) = args.next() {
        if matches!(key.as_str(), "-h" | "--help") {
            println!("history_context_menu_visual --output FRESH_ABSOLUTE_REPO_TMP_DIRECTORY [--theme dark|light] [--scenario text|files|batch|devices|left|narrow] [--exit-after-ms 1000..60000]");
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
                    ["text", "files", "batch", "devices", "left", "narrow"]
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
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
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
        "Repository .tmp must remain inside the repository"
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
    // Open without any native services, then allow the real, read-only
    // foreground eligibility checks used by the production menu registration.
    // No NativeHost, capture loop, hooks or listener is constructed here.
    cliphost_core::force_headless(false);
    std::env::set_var("CLIPHOST_TAURI_HEADLESS", "0");
    ensure!(
        host.lan_status()["enabled"] == false && host.lan_listener_port().is_none(),
        "Fresh core enabled LAN"
    );
    let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
    let report_path = run.join("history-context-menu-report.json");
    let capture_path = run.join("history-context-menu.png");
    write_report(
        &report_path,
        &json!({"pid":std::process::id(),"complete":false,"scenario":scenario,"dataRoot":data}),
    )?;
    Application::new().with_assets(DesignAssets).run(move |cx| {
        let before = unsafe { GetForegroundWindow().0 as usize };
        let setup = (|| -> anyhow::Result<(WindowHandle<ContextMenuPopup>, u64, (i32, i32, i32, i32))> {
            let owner = cx.open_window(WindowOptions { titlebar: None, focus: false, show: false,
                is_resizable: false, is_minimizable: false, ..Default::default() }, |window, cx| {
                    window.set_window_title("ClipHost · isolated history menu owner"); cx.new(|_| Owner)
                })?;
            let owner_hwnd = owner.update(cx, |_, window, _| platform::hwnd(window).map(|hwnd| hwnd.0 as isize).map_err(|error| anyhow!(error)))??;
            host.set_host_hwnd(owner_hwnd);
            host.set_window_runtime_ready(true); host.set_clipboard_web_view_ready(true);
            host.request_show();
            let lease = crate::menu::next_lease();
            ensure!(host.acquire_native_context_menu(lease, "clipboard"), "Fixture lease was refused");
            let area = overlay_work_area_from_point(1000, 250).context("No fixture monitor")?;
            let scale = area.dpi_scale_y;
            let actual = (area.work.left.round() as i32, area.work.top.round() as i32,
                area.work.width.round() as i32, area.work.height.round() as i32);
            let dip = |value: f64| (value * scale).round() as i32;
            let (anchor, work) = if scenario == "narrow" {
                let width = dip(320.0).min(actual.2); let height = dip(180.0).min(actual.3);
                let left = actual.0 + (actual.2 - width) / 2; let top = actual.1 + (actual.3 - height) / 2;
                ((left + dip(100.0), top + dip(55.0)), (left, top, width, height))
            } else if scenario == "left" {
                ((actual.0 + actual.2 - dip(25.0), actual.1 + dip(150.0)), actual)
            } else {
                ((actual.0 + dip(180.0), actual.1 + dip(150.0)), actual)
            };
            let (entries, context, peers) = sample(&scenario);
            let groups = Groups::new(entries);
            let layout = Layout::place(anchor, work, scale, groups.height(super::model::ROOT_WIDTH, context.count > 1), peers.len());
            let finish_host = host.clone(); let finish_report = report_path.clone();
            let finish: HistorySelectHandler = Rc::new(move |lease, selection, cx| {
                finish_host.release_native_context_menu(lease);
                let mut report = fs::read(&finish_report).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()).unwrap_or_else(|| json!({}));
                report["complete"] = json!(true); report["aborted"] = json!(true);
                report["selection"] = json!(format!("{selection:?}"));
                let foreground = unsafe { GetForegroundWindow().0 as usize };
                report["foregroundBefore"] = json!(before); report["foregroundAfter"] = json!(foreground);
                report["foregroundUnchanged"] = json!(before == foreground);
                let _ = write_report(&finish_report, &report); cx.quit();
            });
            let mut existing = None;
            ContextMenuPopup::show_history_layout(&mut existing, &host, owner_hwnd, lease,
                groups, context, peers, layout, tokens, theme == "dark", finish, cx).map_err(|error| anyhow!(error))?;
            let popup = existing.context("Fixture popup was not created")?;
            popup.update(cx, |_, window, _| {
                window.set_window_title("ClipHost · history context menu isolated fixture");
            })?;
            Ok((popup, lease, work))
        })();
        let (popup, lease, work) = match setup { Ok(value) => value, Err(error) => {
            let _ = write_report(&report_path, &json!({"complete":true,"error":format!("{error:#}")}));
            cx.quit(); return;
        }};
        cx.spawn(async move |cx| {
            let started = Instant::now(); let mut ever_foreground = false; let mut foreground_changed = false; let mut capture = None;
            let hold_child = matches!(scenario.as_str(), "devices" | "left" | "narrow");
            loop {
                Timer::after(Duration::from_millis(80)).await;
                let elapsed = started.elapsed();
                let report = popup.update(cx, |view, window, cx| -> anyhow::Result<Value> {
                    let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
                    let after = unsafe { GetForegroundWindow().0 as usize };
                    ever_foreground |= after == hwnd.0 as usize;
                    foreground_changed |= after != before;
                    // Wait for the real LAN row canvas before placing the child.
                    // This avoids capturing a top-aligned provisional child while
                    // the report already reflects its later measured row position.
                    let open_child = hold_child && view.history.as_ref().is_some_and(|history| {
                        !history.devices && history.row_bounds.borrow().contains_key("lan-row")
                    });
                    if open_child { view.history_devices(true, false, window, cx); }
                    let history = view.history.as_mut().context("Fixture history state disappeared")?;
                    // Static child scenes deliberately freeze the production leave timer,
                    // rather than moving the user's pointer into the menu.
                    if hold_child { history.outside_since = None; }
                    let child_painted = !hold_child || history.card_regions.borrow().contains_key(&1);
                    if capture.is_none() && child_painted && elapsed >= Duration::from_millis(640) {
                        capture = Some(capture_window(hwnd, &capture_path).unwrap_or_else(|error| json!({"method":"PrintWindow","success":false,"error":format!("{error:#}")})));
                    }
                    let cards: Vec<_> = history.card_regions.borrow().iter().map(|(index, rect)| json!({"index":index,"rectDip":rect})).collect();
                    let rows: Vec<_> = history.row_bounds.borrow().iter().map(|(key, rect)| json!({"key":key,"rectDip":rect})).collect();
                    let child = history.devices.then(|| {
                        let row_top = history.row_bounds.borrow().get("lan-row").map(|rect| rect.1).unwrap_or(history.layout.root.1);
                        history.layout.devices(row_top)
                    });
                    Ok(json!({"pid":std::process::id(),"title":"ClipHost · history context menu isolated fixture",
                        "popupHwnd":hwnd.0 as usize,"ownerHwnd":history.host.host_hwnd(),"lease":lease,
                        "theme":if view.dark {"dark"} else {"light"},"scenario":scenario,"rectPx":history.layout.window,
                        "workAreaPx":work,"scale":history.layout.scale,"rootRectDip":history.layout.root,
                        "childRectDip":child,"childOpen":history.devices,"childHold":hold_child,"opensLeft":history.layout.opens_left,
                        "cards":cards,"rowBounds":rows,"region":window_region(hwnd,history.layout.window),
                        "sourceCount":history.context.count,"peerCount":history.peers.len(),
                        "peers":history.peers.iter().map(|peer|json!({"id":peer.id,"name":peer.name,"online":peer.online,"paired":peer.paired})).collect::<Vec<_>>(),
                        "dataRoot":data,"noActivate":unsafe {GetWindowLongPtrW(hwnd,GWL_EXSTYLE)} & WS_EX_NOACTIVATE.0 as isize != 0,
                        "foregroundBefore":before,"foregroundAfter":after,"foregroundUnchanged":!foreground_changed,"foregroundPollMs":80,
                        "everForeground":ever_foreground,"aborted":ever_foreground,"complete":elapsed>=Duration::from_millis(exit_after)||ever_foreground,"elapsedMs":elapsed.as_millis(),
                        "capture":capture,"headless":false,"coreOpenedHeadless":true,"nativeHostStarted":false,
                        "globalInputInjected":false,"hooksStarted":false,"clipboardCaptureStarted":false,"networkListenerStarted":false,"actionsExecuted":0}))
                });
                match report {
                    Ok(Ok(report)) => {
                        if write_report(&report_path, &report).is_err() || report["complete"] == true || ever_foreground {
                            host.release_native_context_menu(lease); let _ = cx.update(|cx| cx.quit()); break;
                        }
                    }
                    error => {
                        let _ = write_report(&report_path, &json!({"complete":true,"error":format!("{error:?}")}));
                        host.release_native_context_menu(lease); let _ = cx.update(|cx| cx.quit()); break;
                    }
                }
            }
        }).detach();
    });
    Ok(())
}

// Small direct Win32 bindings keep this Debug fixture independent of additional
// windows crate feature flags. Only the fixture's own HWND/DCs are inspected.
#[link(name = "gdi32")]
extern "system" {
    #[link_name = "CreateCompatibleDC"]
    fn create_dc(source: *mut c_void) -> *mut c_void;
    #[link_name = "CreateCompatibleBitmap"]
    fn create_bitmap(source: *mut c_void, width: i32, height: i32) -> *mut c_void;
    #[link_name = "SelectObject"]
    fn select_object(dc: *mut c_void, object: *mut c_void) -> *mut c_void;
    #[link_name = "DeleteObject"]
    fn delete_object(object: *mut c_void) -> i32;
    #[link_name = "DeleteDC"]
    fn delete_dc(dc: *mut c_void) -> i32;
    #[link_name = "PatBlt"]
    fn clear_bitmap(
        dc: *mut c_void,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        operation: u32,
    ) -> i32;
    #[link_name = "GetDIBits"]
    fn get_bits(
        dc: *mut c_void,
        bitmap: *mut c_void,
        start: u32,
        lines: u32,
        pixels: *mut c_void,
        info: *mut BITMAPINFO,
        usage: u32,
    ) -> i32;
    #[link_name = "CreateRectRgn"]
    fn create_region(left: i32, top: i32, right: i32, bottom: i32) -> *mut c_void;
    #[link_name = "PtInRegion"]
    fn point_in_region(region: *mut c_void, x: i32, y: i32) -> i32;
}
#[link(name = "user32")]
extern "system" {
    #[link_name = "GetDC"]
    fn get_dc(hwnd: *mut c_void) -> *mut c_void;
    #[link_name = "ReleaseDC"]
    fn release_dc(hwnd: *mut c_void, dc: *mut c_void) -> i32;
    #[link_name = "PrintWindow"]
    fn print_window(hwnd: *mut c_void, dc: *mut c_void, flags: u32) -> i32;
    #[link_name = "GetWindowRgn"]
    fn get_region(hwnd: *mut c_void, region: *mut c_void) -> i32;
}

fn window_region(hwnd: HWND, rect: (i32, i32, i32, i32)) -> Value {
    unsafe {
        let region = create_region(0, 0, 0, 0);
        if region.is_null() {
            return json!({"error":"CreateRectRgn failed"});
        }
        let kind = get_region(hwnd.0, region);
        let probes = [
            (0, 0),
            (rect.2 - 1, 0),
            (0, rect.3 - 1),
            (rect.2 - 1, rect.3 - 1),
        ];
        let corners: Vec<_> = probes
            .into_iter()
            .map(|(x, y)| json!({"pointPx":[x,y],"inside":point_in_region(region,x,y)!=0}))
            .collect();
        delete_object(region);
        json!({"kind":kind,"cornerProbes":corners})
    }
}

fn capture_window(hwnd: HWND, path: &Path) -> anyhow::Result<Value> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.context("GetWindowRect failed")?;
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    ensure!(
        width > 0 && height > 0 && i64::from(width) * i64::from(height) <= 16_000_000,
        "Invalid capture dimensions"
    );
    unsafe {
        let screen = get_dc(std::ptr::null_mut());
        ensure!(!screen.is_null(), "GetDC failed");
        let dc = create_dc(screen);
        if dc.is_null() {
            release_dc(std::ptr::null_mut(), screen);
            return Err(anyhow!("CreateCompatibleDC failed"));
        }
        let bitmap = create_bitmap(screen, width, height);
        if bitmap.is_null() {
            delete_dc(dc);
            release_dc(std::ptr::null_mut(), screen);
            return Err(anyhow!("CreateCompatibleBitmap failed"));
        }
        let old = select_object(dc, bitmap);
        if old.is_null() || old as isize == -1 {
            delete_object(bitmap);
            delete_dc(dc);
            release_dc(std::ptr::null_mut(), screen);
            return Err(anyhow!("SelectObject failed"));
        }
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        let mut attempts = Vec::new();
        let mut success = false;
        for flags in [2u32, 0] {
            select_object(dc, bitmap);
            clear_bitmap(dc, 0, 0, width, height, 0x00000042); // BLACKNESS
            let printed = print_window(hwnd.0, dc, flags) != 0;
            select_object(dc, old); // GetDIBits requires the bitmap to be unselected.
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
            let read = get_bits(
                dc,
                bitmap,
                0,
                height as u32,
                pixels.as_mut_ptr().cast(),
                &mut info,
                0,
            ) == height;
            let non_black = pixels
                .chunks_exact(4)
                .filter(|pixel| pixel[..3].iter().any(|channel| *channel != 0))
                .count();
            let mut colors = std::collections::BTreeSet::new();
            for pixel in pixels.chunks_exact(4) {
                colors.insert([pixel[0], pixel[1], pixel[2]]);
                if colors.len() > 256 {
                    break;
                }
            }
            success = printed && read && non_black > 0 && colors.len() > 8;
            attempts.push(json!({"flags":flags,"returned":printed,"pixelsRead":read,"nonBlackPixels":non_black,"distinctColorsUpTo257":colors.len()}));
            if success {
                break;
            }
        }
        select_object(dc, old);
        delete_object(bitmap);
        delete_dc(dc);
        release_dc(std::ptr::null_mut(), screen);
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
        )
        .context("Saving PrintWindow PNG failed")?;
        Ok(
            json!({"method":"PrintWindow","success":success,"path":path,"width":width,"height":height,"attempts":attempts,"screenCaptureFallback":false}),
        )
    }
}
