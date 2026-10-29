//! Disposable, real two-window favorites drag fixture. Headless core mode
//! disables clipboard capture, global hooks, RPC and OLE; GPUI internal drags
//! still use the production views and atomic core commands.

#[cfg(not(debug_assertions))]
fn main() {
    eprintln!("favorites_drag_visual requires a Debug build");
    std::process::exit(2);
}

#[cfg(debug_assertions)]
fn main() {
    if let Err(error) = fixture::run() {
        eprintln!("favorites_drag_visual: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(debug_assertions)]
mod fixture {
    use std::{fs, path::{Path, PathBuf}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

    use anyhow::{anyhow, bail, Context as _, Result};
    use cliphost_core::{
        extra_settings::{FavorSettings, GlobalAppConfig, PanelSettings},
        i18n::{set_language, LanguageMode},
        model::FavorRecord,
        AppPaths, AppStore, ClipKind,
    };
    use cliphost_gpui::{
        platform,
        preview::{DesignTokens, TOKENS_JSON},
        service::{NativeHost, NativeHostOptions},
        view::{ClipboardPreview, UiModel},
        DesignAssets,
    };
    use gpui::{prelude::*, point, px, size, Application, Bounds, Timer, WindowBounds, WindowOptions};
    use serde_json::{json, Value};

    const A: &str = "drag-fixture-a";
    const B: &str = "drag-fixture-b";
    const SUB: &str = "drag-fixture-sub";
    const DEEP: &str = "drag-fixture-deep";
    const ONE: &str = "drag-fixture-text-1";
    const TWO: &str = "drag-fixture-text-2";
    const THREE: &str = "drag-fixture-text-3";
    const SHARED: &str = "drag-fixture-shared";

    struct Options { dark: bool, style: u8, exit_ms: u64 }
    impl Options {
        fn parse() -> Result<Option<Self>> {
            let mut value = Self { dark:false, style:2, exit_ms:180_000 };
            let mut args = std::env::args().skip(1);
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--theme" => value.dark = match args.next().as_deref() {
                        Some("light") => false, Some("dark") => true,
                        _ => bail!("--theme requires light or dark"),
                    },
                    "--panel-style" => {
                        value.style = args.next().context("--panel-style requires 1..4")?.parse()?;
                        if !(1..=4).contains(&value.style) { bail!("--panel-style requires 1..4"); }
                    }
                    "--exit-after-ms" => {
                        value.exit_ms = args.next().context("--exit-after-ms requires a value")?.parse()?;
                        if !(5_000..=600_000).contains(&value.exit_ms) { bail!("--exit-after-ms must be 5000..600000"); }
                    }
                    "--help" | "-h" => {
                        println!("favorites_drag_visual --theme light|dark --panel-style 1|2|3|4 --exit-after-ms 180000");
                        println!("Fresh data and JSON evidence are confined to repository .tmp/favorites-drag-visual/<run>.");
                        println!("Production favorites and panel widgets; internal GPUI drag only. No OLE/capture/hooks/RPC.");
                        return Ok(None);
                    }
                    _ => bail!("unknown option: {arg}"),
                }
            }
            Ok(Some(value))
        }
    }

    fn create_run() -> Result<(PathBuf, AppPaths)> {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize()?;
        let scratch = repo.join(".tmp");
        fs::create_dir_all(&scratch)?;
        let scratch = scratch.canonicalize()?;
        if scratch.parent() != Some(repo.as_path()) { bail!("repository .tmp must not point outside the repository"); }
        let evidence = scratch.join("favorites-drag-visual");
        fs::create_dir_all(&evidence)?;
        let evidence = evidence.canonicalize()?;
        if !evidence.starts_with(&scratch) { bail!("fixture evidence must stay under repository .tmp"); }
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let run = evidence.join(format!("{}-{nonce}", std::process::id()));
        fs::create_dir(&run)?;
        let paths = AppPaths::from_data_root(run.join("data"));
        paths.ensure()?;
        Ok((run, paths))
    }

    fn record(id: &str, name: &str, parent: Option<&str>, children: Option<&[&str]>) -> FavorRecord {
        let folder = children.is_some();
        let now = chrono::DateTime::from_timestamp(1_792_022_400, 0).unwrap();
        FavorRecord {
            storage_revision:None, id:id.into(), parent_id:parent.map(str::to_owned),
            is_folder:folder, name:name.into(),
            kind:if folder { ClipKind::FavoriteClipItem } else { ClipKind::TextClipItem },
            body_text:(!folder).then(|| format!("{name} · disposable drag fixture")),
            file_paths:Vec::new(), image_path:None,
            children:children.unwrap_or_default().iter().map(|id| (*id).into()).collect(),
            create_time:now, update_time:now, source_clip_id:None,
            html_data:None, html_source_url:None, qq_rich_text:None,
            source_process_name:String::new(), sub_index:0, icon:String::new(), execute_param:String::new(),
        }
    }

    fn seed(paths: &AppPaths, options: &Options) -> Result<()> {
        let rows = vec![
            record(A, "A · 源收藏夹", None, Some(&[ONE, TWO, THREE, SUB, SHARED])),
            record(B, "B · 目标收藏夹", None, Some(&[SHARED])),
            record(SUB, "子目录 · 一级", Some(A), Some(&[DEEP])),
            record(DEEP, "子目录 · 二级（空）", Some(SUB), Some(&[])),
            record(ONE, "文本一 · 拖动排序", Some(A), None),
            record(TWO, "文本二 · Ctrl 复制", Some(A), None),
            record(THREE, "文本三 · 多选移动", Some(A), None),
            record(SHARED, "共享文本 · A 与 B 引用同一项", Some(A), None),
        ];
        let store = AppStore::open(&paths.db)?;
        // save_favors_atomic updates existing rows. This database is brand new,
        // so create the fixture records before the host opens it.
        for row in &rows { store.insert_favor(row)?; }
        // Insert also maintains ParentId links; restore the authored Children
        // order and shared A/B edge once every referenced row exists.
        store.save_favors_atomic(&rows)?;
        drop(store);
        let global = GlobalAppConfig {
            ui_theme_mode:if options.dark { "dark" } else { "light" }.into(),
            ui_language:"zh-CN".into(), launch_at_startup:false, run_as_administrator:false,
            window_background_frosted_glass:false, ..Default::default()
        };
        let favor = FavorSettings { root_children:vec![A.into(), B.into()], ..Default::default() };
        let panel = PanelSettings {
            panel_style:format!("PanelStyle{}", options.style), default_panel_favor_id:B.into(),
            show_window_location1:"NA".into(), show_window_location2:"NA".into(), ..Default::default()
        };
        for (name, value) in [
            ("GlobalConfig.json", serde_json::to_value(global)?),
            ("FavorConfig.json", serde_json::to_value(favor)?),
            ("PanelConfig.json", serde_json::to_value(panel)?),
        ] { fs::write(paths.configs.join(name), serde_json::to_vec_pretty(&value)?)?; }
        Ok(())
    }

    #[cfg(windows)]
    fn position(hwnd: isize, left: i32, top: i32) -> Result<Value> {
        use windows::Win32::{Foundation::{HWND, RECT}, UI::WindowsAndMessaging::{GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER}};
        if hwnd == 0 { bail!("fixture window has no HWND"); }
        let hwnd = HWND(hwnd as *mut _);
        unsafe {
            SetWindowPos(hwnd, None, left, top, 520, 660, SWP_NOACTIVATE | SWP_NOZORDER)?;
            let mut rect = RECT::default();
            GetWindowRect(hwnd, &mut rect)?;
            Ok(json!({"left":rect.left,"top":rect.top,"width":rect.right-rect.left,"height":rect.bottom-rect.top}))
        }
    }
    #[cfg(not(windows))]
    fn position(_: isize, _: i32, _: i32) -> Result<Value> { Ok(Value::Null) }

    #[cfg(windows)]
    fn rectangle(hwnd: isize) -> Option<Value> {
        use windows::Win32::{Foundation::{HWND, RECT}, UI::WindowsAndMessaging::GetWindowRect};
        if hwnd == 0 { return None; }
        let mut rect = RECT::default();
        unsafe { GetWindowRect(HWND(hwnd as *mut _), &mut rect).ok()?; }
        Some(json!({"left":rect.left,"top":rect.top,"width":rect.right-rect.left,"height":rect.bottom-rect.top}))
    }
    #[cfg(not(windows))]
    fn rectangle(_: isize) -> Option<Value> { None }

    pub fn run() -> Result<()> {
        let Some(options) = Options::parse()? else { return Ok(()); };
        let (run, paths) = create_run()?;
        seed(&paths, &options)?;
        // Process-wide headless mode remains on until exit. It is never toggled
        // off to simulate a native OLE or physical clipboard acceptance result.
        cliphost_core::force_headless(true);
        set_language(LanguageMode::Chinese);
        let service = NativeHost::open_with_options(NativeHostOptions {
            data_root:Some(paths.data_root.clone()), api_port:0,
            headless:true, background:false, isolated:true, verb_rpc:false, core_copy_acceptance:false,
        }).map_err(|error| anyhow!(error))?;
        let host = service.state();
        host.remember_host_window_rect("clipboard", 60.0, 80.0, 420.0, 528.0);
        host.remember_host_window_rect("panel", 520.0, 80.0, 420.0, 528.0);
        host.config_update(&json!({"showWindowLocation":"NA"}))
            .map_err(|error| anyhow!(error))?;
        host.apply_window_action_for("setPinned", true, "clipboard");
        host.apply_window_action_for("setPinned", true, "panel");
        host.show_web_route(&format!("/favorites?folderId={A}"));
        host.panel_show(&json!({"favorId":B})).map_err(|error| anyhow!(error))?;
        let seed_tree = host.favor_tree().map_err(|error| anyhow!(error))?;
        fs::write(run.join("seed-tree.json"), serde_json::to_vec_pretty(&seed_tree)?)?;
        fs::write(run.join("fixture.json"), serde_json::to_vec_pretty(&json!({
            "dataRoot":paths.data_root,"apiPort":host.api_port(),"ids":{
                "a":A,"b":B,"sub":SUB,"deep":DEEP,"one":ONE,"two":TWO,"three":THREE,"shared":SHARED},
            "headless":true,"isolated":true,"ole":false,"capture":false,"hooks":false,"rpc":false,
            "initialMainFolder":A,"initialPanelFolder":B,"panelStyle":options.style,
            "intendedPhysicalWindows":{"main":{"left":60,"top":80,"width":520,"height":660},"panel":{"left":650,"top":80,"width":520,"height":660}},
            "evidence":run,"report":run.join("report.json"),"seedTree":run.join("seed-tree.json"),"finalTree":run.join("final-tree.json"),
        }))?)?;
        println!("{}", json!({"fixture":"favorites-drag-visual","pid":std::process::id(),"evidence":run,"apiPort":host.api_port(),"ole":false}));
        let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
        let started = Instant::now();
        let live = service.clone();
        let root = run.clone();
        let final_host = host.clone();
        Application::new().with_assets(DesignAssets).run(move |cx| {
            let window = cx.open_window(WindowOptions {
                titlebar:None,
                window_bounds:Some(WindowBounds::Windowed(Bounds::new(point(px(60.0), px(80.0)), size(px(420.0), px(528.0))))),
                focus:false, show:true, window_min_size:Some(size(px(300.0), px(250.0))), ..Default::default()
            }, move |window, cx| cx.new(|cx| {
                let mut preview = ClipboardPreview::new(tokens, UiModel::Live(live), options.dark,
                    "all".into(), String::new(), true, false, started, window, cx);
                preview.enable_diagnostics();
                preview
            })).expect("create disposable real favorites host");
            cx.spawn(async move |cx| {
                let mut positioned = false;
                let mut routed_after_mount = false;
                let mut windows = Value::Null;
                loop {
                    Timer::after(Duration::from_millis(250)).await;
                    if !routed_after_mount && started.elapsed() >= Duration::from_millis(500) {
                        // ClipboardPreview initializes its live history page
                        // during construction. Route after mount through the
                        // same public navigation entry its service pump reads.
                        host.apply_window_action_for("setPinned", true, "clipboard");
                        host.show_web_route(&format!("/favorites?folderId={A}"));
                        routed_after_mount = true;
                    }
                    let panel_hwnd = host.panel_native_snapshot()["hwnd"].as_i64().unwrap_or(0) as isize;
                    if !positioned && started.elapsed() >= Duration::from_millis(1200) && panel_hwnd != 0 {
                        windows = json!({
                            "main":{"hwnd":host.host_hwnd(),"rect":position(host.host_hwnd(),60,80).ok()},
                            "panel":{"hwnd":panel_hwnd,"rect":position(panel_hwnd,650,80).ok()}
                        });
                        println!("{}", json!({"fixtureWindows":windows,"evidence":root}));
                        positioned = true;
                    }
                    if positioned {
                        windows = json!({
                            "main":{"hwnd":host.host_hwnd(),"rect":rectangle(host.host_hwnd())},
                            "panel":{"hwnd":panel_hwnd,"rect":rectangle(panel_hwnd)}
                        });
                    }
                    let Some(report) = window.update(cx, |view, window, cx| {
                        let viewport = window.viewport_size();
                        json!({"fixture":"favorites-drag-visual","pid":std::process::id(),"elapsedMs":started.elapsed().as_millis(),
                            "headless":true,"isolated":true,"ole":false,"capture":false,"hooks":false,"rpc":false,
                            "windows":windows,"mainHwnd":host.host_hwnd(),"panel":host.panel_native_snapshot(),
                            "favoritesRouteRequestedAfterMount":routed_after_mount,
                            "viewport":{"width":f32::from(viewport.width),"height":f32::from(viewport.height),"scaleFactor":window.scale_factor()},
                            "tree":host.favor_tree().ok(),"main":view.report(window,cx),"nativeError":platform::take_error(window).ok().flatten()})
                    }).ok() else { break; };
                    if let Ok(bytes) = serde_json::to_vec_pretty(&report) { let _ = fs::write(root.join("report.json"), bytes); }
                    if started.elapsed() >= Duration::from_millis(options.exit_ms) {
                        let _ = cx.update(|cx| cx.quit());
                        break;
                    }
                }
            }).detach();
        });
        let final_tree = final_host.favor_tree().map_err(|error| anyhow!(error))?;
        fs::write(run.join("final-tree.json"), serde_json::to_vec_pretty(&final_tree)?)?;
        service.shutdown();
        println!("{}", json!({"completed":true,"evidence":run,"finalTree":run.join("final-tree.json"),"ole":false}));
        Ok(())
    }
}
