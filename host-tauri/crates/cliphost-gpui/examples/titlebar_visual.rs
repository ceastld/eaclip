//! Disposable single-window titlebar rendering and native hit-test evidence.
//! Core remains headless; only this fixture's HWND is shown without activation.

#[cfg(not(debug_assertions))]
fn main() {
    eprintln!("titlebar_visual requires a Debug build");
    std::process::exit(2);
}

#[cfg(debug_assertions)]
fn main() {
    if let Err(error) = fixture::run() {
        eprintln!("titlebar_visual: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(debug_assertions)]
mod fixture {
    use std::{
        fs,
        path::{Component, Path, PathBuf},
        time::{Duration, Instant},
    };

    use anyhow::{anyhow, bail, Context as _, Result};
    use cliphost_core::{
        extra_settings::{FavorSettings, GlobalAppConfig},
        i18n::{set_language, LanguageMode},
        model::{ClipRecord, FavorRecord},
        quick_paste::KeyEvent,
        AppPaths, AppStore, ClipKind, ClipboardKeyboardMode, HostState,
    };
    use cliphost_gpui::{
        platform,
        preview::{DesignTokens, TOKENS_JSON},
        service::{NativeHost, NativeHostOptions},
        titlebar_layout::history_titlebar_layout,
        view::{ClipboardPreview, UiModel},
        DesignAssets,
    };
    use gpui::{
        point, prelude::*, px, size, Application, Bounds, Timer, WindowBounds, WindowOptions,
    };
    use serde_json::{json, Value};

    struct Options {
        dark: bool,
        width: f32,
        height: f32,
        favorites: bool,
        verify_focus: bool,
        verify_keyboard_ownership: bool,
        output: PathBuf,
        exit_ms: u64,
    }

    impl Options {
        fn parse() -> Result<Option<Self>> {
            let mut value = Self {
                dark: true,
                width: 420.0,
                height: 560.0,
                favorites: false,
                verify_focus: false,
                verify_keyboard_ownership: false,
                output: PathBuf::new(),
                exit_ms: 5_000,
            };
            let mut args = std::env::args().skip(1);
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--theme" => {
                        value.dark = match args.next().as_deref() {
                            Some("dark") => true,
                            Some("light") => false,
                            _ => bail!("--theme requires dark or light"),
                        }
                    }
                    "--width" => {
                        value.width = args.next().context("--width requires a value")?.parse()?
                    }
                    "--height" => {
                        value.height = args.next().context("--height requires a value")?.parse()?
                    }
                    "--view" => {
                        value.favorites = match args.next().as_deref() {
                            Some("clipboard") => false,
                            Some("favorites") => true,
                            _ => bail!("--view requires clipboard or favorites"),
                        }
                    }
                    "--verify-focus" => value.verify_focus = true,
                    "--verify-keyboard-ownership" => value.verify_keyboard_ownership = true,
                    "--output" => {
                        value.output = args
                            .next()
                            .context("--output requires an absolute directory")?
                            .into()
                    }
                    "--exit-after-ms" => {
                        value.exit_ms = args
                            .next()
                            .context("--exit-after-ms requires a value")?
                            .parse()?
                    }
                    "--help" | "-h" => {
                        println!("titlebar_visual --theme dark|light --width 300|420 --height 560 --view clipboard|favorites --output <absolute repository .tmp directory> --exit-after-ms 5000 [--verify-focus | --verify-keyboard-ownership]");
                        println!("Fresh data only. No clipboard capture, global hooks, RPC, OLE or global input injection; a private loopback HTTP listener is reported.");
                        println!("--verify-focus sends directed native messages only to this fixture's HWND, blocks native activation, and exits after checks (timeout at least 20000 ms). Physical IME and native dragging are not verified.");
                        println!("--verify-keyboard-ownership simulates Core routing and observes the real GPUI query/status entities. It installs no hooks and sends no native input; physical keyboard, mouse and IME behavior are not verified.");
                        return Ok(None);
                    }
                    _ => bail!("unknown option: {arg}"),
                }
            }
            if !value.width.is_finite()
                || !(300.0..=960.0).contains(&value.width)
                || !value.height.is_finite()
                || !(250.0..=960.0).contains(&value.height)
            {
                bail!("width must be 300..960 and height 250..960 DIP");
            }
            if !(2_000..=60_000).contains(&value.exit_ms) {
                bail!("--exit-after-ms must be 2000..60000");
            }
            if !value.output.is_absolute() {
                bail!("--output requires an absolute path under this repository's .tmp");
            }
            if value.verify_keyboard_ownership && (value.verify_focus || value.favorites) {
                bail!("--verify-keyboard-ownership requires --view clipboard and cannot combine with --verify-focus");
            }
            #[cfg(not(windows))]
            if value.verify_focus || value.verify_keyboard_ownership {
                bail!("native titlebar verification requires Windows");
            }
            Ok(Some(value))
        }
    }

    fn create_data(output: &Path) -> Result<(PathBuf, AppPaths)> {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()?;
        let scratch = repo.join(".tmp");
        fs::create_dir_all(&scratch)?;
        let scratch = scratch.canonicalize()?;
        if scratch.parent() != Some(repo.as_path()) {
            bail!("repository .tmp must not resolve outside the repository");
        }
        if output
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        {
            bail!("--output must not contain parent traversal");
        }
        let existing_parent = output
            .ancestors()
            .find(|path| path.exists())
            .context("output has no existing ancestor")?
            .canonicalize()?;
        if !existing_parent.starts_with(&scratch) {
            bail!("--output must remain under the canonical repository .tmp");
        }
        fs::create_dir_all(output)?;
        let output = output.canonicalize()?;
        if output == scratch || !output.starts_with(&scratch) {
            bail!("--output must be a child of the repository .tmp");
        }
        // Refuse an existing database or linked data directory, rather than
        // overwriting evidence or opening another run's persisted records.
        let data = output.join("data");
        fs::create_dir(&data).context("output/data must be fresh")?;
        let paths = AppPaths::from_data_root(data);
        paths.ensure()?;
        Ok((output, paths))
    }

    fn favor(id: &str, name: &str, parent: Option<&str>, children: Option<&[&str]>) -> FavorRecord {
        let now = chrono::DateTime::from_timestamp(1_792_022_400, 0).unwrap();
        FavorRecord {
            storage_revision: None,
            id: id.into(),
            parent_id: parent.map(str::to_owned),
            is_folder: children.is_some(),
            name: name.into(),
            kind: if children.is_some() {
                ClipKind::FavoriteClipItem
            } else {
                ClipKind::TextClipItem
            },
            body_text: children
                .is_none()
                .then(|| format!("{name} · 标题栏隔离样例")),
            file_paths: vec![],
            image_path: None,
            children: children
                .unwrap_or_default()
                .iter()
                .map(|id| (*id).into())
                .collect(),
            create_time: now,
            update_time: now,
            source_clip_id: None,
            html_data: None,
            html_source_url: None,
            qq_rich_text: None,
            source_process_name: String::new(),
            sub_index: 0,
            icon: String::new(),
            execute_param: String::new(),
        }
    }

    fn seed(paths: &AppPaths, dark: bool) -> Result<()> {
        let rows = vec![
            favor(
                "titlebar-a",
                "常用文本",
                None,
                Some(&["titlebar-one", "titlebar-two"]),
            ),
            favor("titlebar-b", "项目备忘", None, Some(&["titlebar-three"])),
            favor(
                "titlebar-one",
                "快速复制，轻松回贴。",
                Some("titlebar-a"),
                None,
            ),
            favor(
                "titlebar-two",
                "先对齐已确认的界面，再连接同一 Rust 核心。",
                Some("titlebar-a"),
                None,
            ),
            favor(
                "titlebar-three",
                "本窗口只使用固定样例。",
                Some("titlebar-b"),
                None,
            ),
        ];
        let store = AppStore::open(&paths.db)?;
        for row in &rows {
            store.insert_favor(row)?;
        }
        store.save_favors_atomic(&rows)?;
        for (index, text) in [
            "快速复制，轻松回贴。",
            "搜索和视图入口在标题栏；空白区域用于原生拖动。",
            "暂停记录在左下角，设置在右下角。",
        ]
        .into_iter()
        .enumerate()
        {
            let mut row = ClipRecord::new_text(text, "titlebar-visual.exe");
            row.id = format!("titlebar-history-{index}");
            row.create_time = chrono::Utc::now() - chrono::Duration::minutes(index as i64);
            row.update_time = row.create_time;
            store.insert_clip(&row, true)?;
        }
        drop(store);
        let global = GlobalAppConfig {
            ui_theme_mode: if dark { "dark" } else { "light" }.into(),
            ui_language: "zh-CN".into(),
            launch_at_startup: false,
            run_as_administrator: false,
            window_background_frosted_glass: false,
            ..Default::default()
        };
        let favorites = FavorSettings {
            root_children: vec!["titlebar-a".into(), "titlebar-b".into()],
            ..Default::default()
        };
        fs::write(
            paths.configs.join("GlobalConfig.json"),
            serde_json::to_vec_pretty(&global)?,
        )?;
        fs::write(
            paths.configs.join("FavorConfig.json"),
            serde_json::to_vec_pretty(&favorites)?,
        )?;
        Ok(())
    }

    #[cfg(windows)]
    fn native_report(
        window: &gpui::Window,
        tokens: &DesignTokens,
        favorites: bool,
    ) -> Result<Value> {
        use windows::Win32::{
            Foundation::{LPARAM, POINT, RECT, WPARAM},
            Graphics::Gdi::ClientToScreen,
            UI::WindowsAndMessaging::*,
        };
        let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
        let width = f32::from(window.viewport_size().width);
        let scale = window.scale_factor();
        let l = &tokens.layout;
        let b = tokens.titlebar_search.button_size;
        let sizing = history_titlebar_layout(width, tokens);
        let search_left = l.title_bar_padding_x + 2.0 * b + l.chip_gap + l.title_bar_gap;
        let controls_left = width - l.title_bar_padding_x - 3.0 * b - 2.0 * l.chrome_button_gap;
        let y = l.title_bar_height / 2.0;
        let mut samples = vec![
            ("clipboard", l.title_bar_padding_x + b / 2.0, 1),
            (
                "favorites",
                l.title_bar_padding_x + b + l.chip_gap + b / 2.0,
                1,
            ),
            ("search-left-padding", search_left + 2.0, 1),
            (
                "search-icon",
                search_left
                    + if favorites {
                        10.0
                    } else {
                        3.0 + l.inline_icon_button_size / 2.0
                    },
                1,
            ),
            ("search-middle", search_left + sizing.search_width / 2.0, 1),
            ("search-right", search_left + sizing.search_width - 2.0, 1),
            (
                "drag",
                search_left
                    + sizing.search_width
                    + l.title_bar_gap
                    + if favorites {
                        sizing.drag_width / 2.0
                    } else {
                        12.0
                    },
                2,
            ),
            ("pin", controls_left + b / 2.0, 1),
            (
                "collapse",
                controls_left + b + l.chrome_button_gap + b / 2.0,
                1,
            ),
            (
                "close",
                controls_left + 2.0 * (b + l.chrome_button_gap) + b / 2.0,
                1,
            ),
        ];
        if !favorites {
            samples.push(("keyboard-status", controls_left - l.title_bar_gap - 12.0, 1));
        }
        let mut values = vec![];
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != std::process::id() {
                bail!("native evidence HWND is not owned by this process");
            }
            for (name, x, expected) in samples {
                let client = POINT {
                    x: (x * scale).round() as i32,
                    y: (y * scale).round() as i32,
                };
                let mut screen = client;
                if !ClientToScreen(hwnd, &mut screen).as_bool() {
                    bail!("ClientToScreen failed");
                }
                let packed = ((screen.x as u16 as u32) | ((screen.y as u16 as u32) << 16)) as isize;
                let actual =
                    SendMessageW(hwnd, WM_NCHITTEST, Some(WPARAM(0)), Some(LPARAM(packed))).0;
                values.push(json!({"name":name,"logical":{"x":x,"y":y},"client":{"x":client.x,"y":client.y},
                    "screen":{"x":screen.x,"y":screen.y},"expected":expected,"actual":actual,"passed":actual==expected}));
            }
            let mut rect = RECT::default();
            GetWindowRect(hwnd, &mut rect)?;
            let mut client = RECT::default();
            GetClientRect(hwnd, &mut client)?;
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            let extended = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            Ok(
                json!({"hwnd":hwnd.0 as isize,"pid":pid,"visible":IsWindowVisible(hwnd).as_bool(),
                "foregroundHwnd":GetForegroundWindow().0 as isize,"foregroundIsFixture":GetForegroundWindow()==hwnd,
                "noActivate":extended & WS_EX_NOACTIVATE.0 as isize != 0,
                "toolWindow":extended & WS_EX_TOOLWINDOW.0 as isize != 0,
                "topmost":extended & WS_EX_TOPMOST.0 as isize != 0,
                "resizable":style & WS_THICKFRAME.0 as isize != 0,
                "rect":{"left":rect.left,"top":rect.top,"width":rect.right-rect.left,"height":rect.bottom-rect.top},
                "client":{"width":client.right-client.left,"height":client.bottom-client.top},
                "layout":{"searchWidth":sizing.search_width,"dragWidth":sizing.drag_width},
                "allHitTestsPassed":values.iter().all(|value| value["passed"].as_bool()==Some(true)),"hitTests":values}),
            )
        }
    }

    #[cfg(not(windows))]
    fn native_report(_: &gpui::Window, _: &DesignTokens, _: bool) -> Result<Value> {
        Ok(json!({"supported":false,"reason":"WM_NCHITTEST requires Windows"}))
    }

    #[derive(Clone, Copy)]
    enum FocusAction {
        Observe,
        Tab,
        Click(&'static str),
        Text(&'static str),
    }

    struct FocusStep {
        name: String,
        action: FocusAction,
        page: &'static str,
        search: bool,
        query: &'static str,
    }

    /// This is intentionally a directed-message regression, not SendInput or
    /// physical input. All messages are sent after the GPUI window borrow ends.
    struct FocusScenario {
        steps: Vec<FocusStep>,
        next: usize,
        pending_since: Option<Instant>,
        checks: Vec<Value>,
        failed: bool,
        message_count: usize,
    }

    impl FocusScenario {
        fn new(favorites: bool) -> Self {
            let mut steps = vec![];
            let mut page = if favorites { "favorites" } else { "clipboard" };
            steps.push(FocusStep {
                name: "initial-page-list-focus".into(),
                action: FocusAction::Observe,
                page,
                search: false,
                query: "",
            });
            for index in 0..4 {
                page = if page == "clipboard" {
                    "favorites"
                } else {
                    "clipboard"
                };
                steps.push(FocusStep {
                    name: format!("consecutive-tab-{}-to-{page}", index + 1),
                    action: FocusAction::Tab,
                    page,
                    search: false,
                    query: "",
                });
            }
            if page != "clipboard" {
                steps.push(FocusStep {
                    name: "prepare-history-search".into(),
                    action: FocusAction::Tab,
                    page: "clipboard",
                    search: false,
                    query: "",
                });
            }
            for (page, query) in [
                ("clipboard", "qa_history_314159"),
                ("favorites", "qa_favorite_271828"),
            ] {
                for (name, action, search, expected_query) in [
                    (
                        "click-search",
                        FocusAction::Click("search-middle"),
                        true,
                        "",
                    ),
                    ("type-query", FocusAction::Text(query), true, query),
                    (
                        "click-search-padding",
                        FocusAction::Click("search-left-padding"),
                        true,
                        query,
                    ),
                    (
                        "click-caption-space",
                        FocusAction::Click("drag"),
                        false,
                        query,
                    ),
                    (
                        "refocus-search",
                        FocusAction::Click("search-middle"),
                        true,
                        query,
                    ),
                    (
                        "click-body-space",
                        FocusAction::Click("body-space"),
                        false,
                        query,
                    ),
                ] {
                    steps.push(FocusStep {
                        name: format!("{page}-{name}"),
                        action,
                        page,
                        search,
                        query: expected_query,
                    });
                }
                if page == "clipboard" {
                    steps.push(FocusStep {
                        name: "tab-from-history-query-to-favorites".into(),
                        action: FocusAction::Tab,
                        page: "favorites",
                        search: false,
                        query: "",
                    });
                }
            }
            steps.push(FocusStep {
                name: "return-to-history-retains-query".into(),
                action: FocusAction::Tab,
                page: "clipboard",
                search: false,
                query: "qa_history_314159",
            });
            steps.push(FocusStep {
                name: "return-to-favorites-retains-query".into(),
                action: FocusAction::Tab,
                page: "favorites",
                search: false,
                query: "qa_favorite_271828",
            });
            if !favorites {
                steps.push(FocusStep {
                    name: "finish-on-requested-history-page".into(),
                    action: FocusAction::Tab,
                    page: "clipboard",
                    search: false,
                    query: "qa_history_314159",
                });
            }
            Self {
                steps,
                next: 0,
                pending_since: None,
                checks: vec![],
                failed: false,
                message_count: 0,
            }
        }

        fn complete(&self) -> bool {
            self.failed || self.next == self.steps.len()
        }

        fn report(&self) -> Value {
            json!({
                "enabled":true,"complete":self.complete(),
                "passed":self.complete() && !self.failed && self.next==self.steps.len(),
                "method":"directed Win32 messages to this process's UI-thread-owned fixture HWND",
                "nativeActivationBlocked":true,"globalInputInjected":false,
                "physicalImeVerified":false,"nativeDragLoopVerified":false,
                "captionEvidence":"WM_NCHITTEST remains HTCAPTION; a client-coordinate MouseDown exercises blur without entering the OS move loop",
                "completedSteps":self.next,"plannedSteps":self.steps.len(),
                "messageCount":self.message_count,
                "keyboardPastePauseChecks":self.checks.iter().filter(|check| check["keyboardPastePause"]["checked"]==true).count(),
                "checks":self.checks
            })
        }

        fn timeout(&mut self) {
            if !self.complete() {
                self.failed = true;
                self.checks
                    .push(json!({"name":"scenario-timeout","passed":false}));
            }
        }

        fn tick(&mut self, snapshot: &Value, tokens: &DesignTokens) {
            if self.complete() {
                return;
            }
            let step = &self.steps[self.next];
            if let Some(since) = self.pending_since {
                let focus = &snapshot["main"]["focus"];
                let (list, search, query) = if step.page == "clipboard" {
                    (
                        &focus["historyList"],
                        &focus["historySearch"],
                        &snapshot["main"]["query"],
                    )
                } else {
                    (
                        &focus["favorites"]["list"],
                        &focus["favorites"]["search"],
                        &focus["favorites"]["query"],
                    )
                };
                let isolated = snapshot["native"]["foregroundIsFixture"] == false
                    && snapshot["native"]["noActivate"] == true
                    && snapshot["native"]["allHitTestsPassed"] == true;
                let keyboard_pause_required = focus["primaryPage"] == "clipboard"
                    && (focus["historySearch"] == true
                        || snapshot["main"]["query"]
                            .as_str()
                            .is_some_and(|query| !query.is_empty()));
                let keyboard_active = snapshot["main"]["keyboardPasteStatus"]["active"].as_bool();
                let keyboard_paused = !keyboard_pause_required || keyboard_active == Some(false);
                let passed = focus["primaryPage"] == step.page
                    && list.as_bool() == Some(!step.search)
                    && search.as_bool() == Some(step.search)
                    && query.as_str() == Some(step.query)
                    && isolated
                    && keyboard_paused;
                // A route/query may finish asynchronously. Observe the result
                // without replaying the click or keystroke while it settles.
                if !passed && isolated && since.elapsed() < Duration::from_millis(1_750) {
                    return;
                }
                self.checks.push(json!({
                    "name":step.name,"passed":passed,
                    "expected":{"primaryPage":step.page,"list":!step.search,"search":step.search,"query":step.query},
                    "actual":{"focus":focus,"query":query},
                    "keyboardPastePause":{"checked":keyboard_pause_required,
                        "expectedActive":if keyboard_pause_required {Some(false)} else {None},
                        "actualActive":keyboard_active,"passed":keyboard_paused},
                    "noActivate":snapshot["native"]["noActivate"],
                    "foregroundIsFixture":snapshot["native"]["foregroundIsFixture"],
                    "captionHitTestsPassed":snapshot["native"]["allHitTestsPassed"]
                }));
                if !passed {
                    self.failed = true;
                    return;
                }
                self.next += 1;
                self.pending_since = None;
                // Leave one settled frame between successive native messages.
                return;
            }
            match send_focus_action(step.action, snapshot, tokens) {
                Ok(count) => {
                    self.message_count += count;
                    self.pending_since = Some(Instant::now());
                }
                Err(error) => {
                    self.failed = true;
                    self.checks
                        .push(json!({"name":step.name,"passed":false,"error":error.to_string()}));
                }
            }
        }
    }

    #[derive(Clone, Copy)]
    enum OwnershipAction {
        Observe,
        Outside,
        PassKey(u32),
        Hover,
        Show,
        AppendQuery,
    }

    struct OwnershipStep {
        name: &'static str,
        action: OwnershipAction,
        mode: ClipboardKeyboardMode,
        active: bool,
        query: &'static str,
        reason: &'static str,
    }

    /// Only direct Core calls are simulated. The retained production GPUI view
    /// consumes the harmless AppendSearch command and publishes its query gate.
    /// No native key/mouse messages, hooks, activation or paste are performed.
    struct OwnershipScenario {
        steps: Vec<OwnershipStep>,
        next: usize,
        pending_since: Option<Instant>,
        action_evidence: Value,
        checks: Vec<Value>,
        failed: bool,
        key_clock: u64,
    }

    impl OwnershipScenario {
        fn new() -> Self {
            use ClipboardKeyboardMode::{DigitsOnly, Full, Released};
            let steps = [
                ("initial-full", OwnershipAction::Observe, Full, true, "", ""),
                (
                    "outside-retains-digits",
                    OwnershipAction::Outside,
                    DigitsOnly,
                    true,
                    "",
                    "",
                ),
                (
                    "first-a-passes-and-releases",
                    OwnershipAction::PassKey(0x41),
                    Released,
                    false,
                    "",
                    "已让出键盘",
                ),
                (
                    "following-1-passes",
                    OwnershipAction::PassKey(0x31),
                    Released,
                    false,
                    "",
                    "已让出键盘",
                ),
                (
                    "hover-does-not-revive",
                    OwnershipAction::Hover,
                    Released,
                    false,
                    "",
                    "已让出键盘",
                ),
                (
                    "explicit-show-restores-full",
                    OwnershipAction::Show,
                    Full,
                    true,
                    "",
                    "",
                ),
                (
                    "gpui-query-publishes-search-gate",
                    OwnershipAction::AppendQuery,
                    Full,
                    false,
                    "a",
                    "正在搜索",
                ),
                (
                    "outside-query-pauses-digits",
                    OwnershipAction::Outside,
                    DigitsOnly,
                    false,
                    "a",
                    "已有搜索条件",
                ),
                (
                    "external-1-preserves-gpui-query",
                    OwnershipAction::PassKey(0x31),
                    DigitsOnly,
                    false,
                    "a",
                    "已有搜索条件",
                ),
            ]
            .into_iter()
            .map(
                |(name, action, mode, active, query, reason)| OwnershipStep {
                    name,
                    action,
                    mode,
                    active,
                    query,
                    reason,
                },
            )
            .collect();
            Self {
                steps,
                next: 0,
                pending_since: None,
                action_evidence: Value::Null,
                checks: vec![],
                failed: false,
                key_clock: 1,
            }
        }

        fn complete(&self) -> bool {
            self.failed || self.next == self.steps.len()
        }

        fn report(&self) -> Value {
            json!({"enabled":true,"complete":self.complete(),
                "passed":self.complete() && !self.failed && self.next==self.steps.len(),
                "method":"direct Core routing simulation with the retained real GPUI query and keyboard status entities",
                "globalHooks":false,"nativeInputMessages":false,"globalInputInjected":false,
                "nativeActivationBlocked":true,"physicalKeyboardVerified":false,
                "physicalMouseVerified":false,"physicalImeVerified":false,"pastePerformed":false,
                "searchGateEvidence":"first observed nonempty GPUI query must already disable Core digit routing; external digit must queue no command and leave that query unchanged",
                "completedSteps":self.next,"plannedSteps":self.steps.len(),"checks":self.checks})
        }

        fn timeout(&mut self) {
            if !self.complete() {
                self.failed = true;
                self.checks
                    .push(json!({"name":"scenario-timeout","passed":false,
                    "pendingStep":self.steps[self.next].name,"action":self.action_evidence}));
            }
        }

        fn drain(host: &HostState) -> Vec<String> {
            std::iter::from_fn(|| host.take_chrome_task())
                .map(|task| format!("{:?}", task.command()))
                .collect()
        }

        fn tick(&mut self, snapshot: &Value, host: &HostState) {
            if self.complete() {
                return;
            }
            let step = &self.steps[self.next];
            if let Some(since) = self.pending_since {
                let marker = &snapshot["main"]["keyboardPasteStatus"];
                let reason = marker["inactiveReason"].as_str().unwrap_or("");
                let isolated = snapshot["native"]["foregroundIsFixture"] == false
                    && snapshot["native"]["noActivate"] == true
                    && snapshot["native"]["allHitTestsPassed"] == true;
                let query = snapshot["main"]["query"].as_str();
                let core_digits = host.clipboard_digit_routing_active();
                let passed = isolated
                    && host.clipboard_keyboard_mode() == step.mode
                    && marker["active"].as_bool() == Some(step.active)
                    && marker["visible"] == true
                    && marker["pinned"] == true
                    && query == Some(step.query)
                    && reason.contains(step.reason)
                    && core_digits == step.active;
                // Observe asynchronous list/query rendering without replaying
                // the action. Query publication is checked on its first report.
                let query_gate_late =
                    !step.query.is_empty() && query == Some(step.query) && core_digits;
                if !passed
                    && !query_gate_late
                    && isolated
                    && since.elapsed() < Duration::from_secs(4)
                {
                    return;
                }
                self.checks.push(json!({"name":step.name,"passed":passed,
                    "expected":{"mode":format!("{:?}",step.mode),"markerActive":step.active,
                        "query":step.query,"reasonContains":step.reason},
                    "actual":{"mode":format!("{:?}",host.clipboard_keyboard_mode()),
                        "coreDigitRoutingActive":core_digits,"query":query,"status":marker},
                    "action":self.action_evidence,"queryGateLate":query_gate_late,
                    "noActivate":snapshot["native"]["noActivate"],
                    "foregroundIsFixture":snapshot["native"]["foregroundIsFixture"]}));
                if !passed {
                    self.failed = true;
                    return;
                }
                self.next += 1;
                self.pending_since = None;
                return;
            }
            let before_mode = host.clipboard_keyboard_mode();
            // Discard lifecycle notifications before each simulated action.
            // In particular, an explicit Show must never reach native placement.
            let before_commands = Self::drain(host);
            let mut evidence = json!({"beforeMode":format!("{before_mode:?}"),
                "discardedBeforeCommands":before_commands});
            let mut action_passed = true;
            match step.action {
                OwnershipAction::Observe => {}
                OwnershipAction::Outside => {
                    host.set_pointer_in_host(false);
                    host.notify_outside_click_at(99_999, 99_999);
                    evidence["simulatedOutsidePoint"] = json!([99_999, 99_999]);
                    evidence["discardedCommands"] = json!(Self::drain(host));
                }
                OwnershipAction::Hover => host.set_pointer_in_host(true),
                OwnershipAction::Show => {
                    host.set_pointer_in_host(false);
                    host.request_show();
                    evidence["discardedCommands"] = json!(Self::drain(host));
                }
                OwnershipAction::PassKey(_) | OwnershipAction::AppendQuery => {
                    let vk = match step.action {
                        OwnershipAction::PassKey(vk) => vk,
                        _ => 0x41,
                    };
                    let event = KeyEvent {
                        vk,
                        down: true,
                        alt: false,
                        ctrl: false,
                        shift: false,
                        win: false,
                        now_ms: self.key_clock,
                    };
                    let down = host.dispatch_low_level_key(event);
                    let up = host.dispatch_low_level_key(KeyEvent {
                        down: false,
                        now_ms: self.key_clock + 1,
                        ..event
                    });
                    self.key_clock += 2;
                    let append = matches!(step.action, OwnershipAction::AppendQuery);
                    evidence["key"] = json!({"vk":vk,"downConsumed":down,"upConsumed":up,
                        "expectedDownConsumed":append,"expectedUpConsumed":false});
                    action_passed = down == append && !up;
                    if !append {
                        // Drain even on a regression before GPUI can consume an
                        // unexpected RouteDigit and perform a real paste.
                        let commands = Self::drain(host);
                        action_passed &= commands.is_empty();
                        evidence["unexpectedCommands"] = json!(commands);
                    } else {
                        evidence["allowedGpuiCommand"] = json!("ClipboardAppendSearch('a')");
                    }
                }
            }
            evidence["afterMode"] = json!(format!("{:?}", host.clipboard_keyboard_mode()));
            self.action_evidence = evidence;
            if !action_passed {
                self.failed = true;
                self.checks.push(json!({"name":step.name,"passed":false,
                    "failure":"direct Core routing result or queued command differed",
                    "action":self.action_evidence}));
                return;
            }
            self.pending_since = Some(Instant::now());
        }
    }

    #[cfg(windows)]
    fn send_focus_action(
        action: FocusAction,
        snapshot: &Value,
        tokens: &DesignTokens,
    ) -> Result<usize> {
        use windows::Win32::{
            Foundation::{HWND, LPARAM, WPARAM},
            System::Threading::GetCurrentThreadId,
            UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
        };
        let hwnd = HWND(
            snapshot["native"]["hwnd"]
                .as_i64()
                .context("fixture HWND missing")? as *mut _,
        );
        unsafe {
            let mut pid = 0;
            let thread = GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != std::process::id()
                || thread != GetCurrentThreadId()
                || !IsWindow(Some(hwnd)).as_bool()
            {
                bail!("refusing input: target is not this fixture's current UI-thread-owned HWND");
            }
            if GetForegroundWindow() == hwnd
                || GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_NOACTIVATE.0 as isize == 0
            {
                bail!("refusing input: fixture must remain non-foreground and WS_EX_NOACTIVATE");
            }
            // Do not mix a directed regression with any physical modifier or
            // pointer press the user currently holds. This only reads state.
            for key in [
                VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN, VK_LBUTTON, VK_RBUTTON, VK_MBUTTON,
            ] {
                if GetAsyncKeyState(i32::from(key.0)) < 0 {
                    bail!("refusing input while a physical modifier or pointer button is held");
                }
            }
            match action {
                FocusAction::Observe => Ok(0),
                FocusAction::Tab => {
                    SendMessageW(
                        hwnd,
                        WM_KEYDOWN,
                        Some(WPARAM(VK_TAB.0 as usize)),
                        Some(LPARAM(1 | (0x0f << 16))),
                    );
                    SendMessageW(
                        hwnd,
                        WM_KEYUP,
                        Some(WPARAM(VK_TAB.0 as usize)),
                        Some(LPARAM(
                            (1u32 | (0x0f << 16) | (1 << 30) | (1 << 31)) as isize,
                        )),
                    );
                    Ok(2)
                }
                FocusAction::Text(text) => {
                    for unit in text.encode_utf16() {
                        SendMessageW(hwnd, WM_CHAR, Some(WPARAM(unit as usize)), Some(LPARAM(1)));
                    }
                    Ok(text.encode_utf16().count())
                }
                FocusAction::Click(name) => {
                    let (x, y) = if name == "body-space" {
                        (
                            snapshot["main"]["width"]
                                .as_f64()
                                .context("viewport width missing")?
                                - 28.0,
                            snapshot["main"]["height"]
                                .as_f64()
                                .context("viewport height missing")?
                                - f64::from(tokens.layout.footer_height)
                                - 32.0,
                        )
                    } else {
                        let target = snapshot["native"]["hitTests"]
                            .as_array()
                            .context("native hit tests missing")?
                            .iter()
                            .find(|value| value["name"] == name)
                            .context("click target missing")?;
                        (
                            target["logical"]["x"].as_f64().context("click x missing")?,
                            target["logical"]["y"].as_f64().context("click y missing")?,
                        )
                    };
                    let scale = snapshot["viewport"]["scaleFactor"]
                        .as_f64()
                        .context("scale factor missing")?;
                    let x = (x * scale).round() as i32;
                    let y = (y * scale).round() as i32;
                    let packed = LPARAM(((x as u16 as u32) | ((y as u16 as u32) << 16)) as isize);
                    SendMessageW(hwnd, WM_MOUSEMOVE, Some(WPARAM(0)), Some(packed));
                    SendMessageW(hwnd, WM_LBUTTONDOWN, Some(WPARAM(1)), Some(packed));
                    SendMessageW(hwnd, WM_LBUTTONUP, Some(WPARAM(0)), Some(packed));
                    Ok(3)
                }
            }
        }
    }

    #[cfg(not(windows))]
    fn send_focus_action(_: FocusAction, _: &Value, _: &DesignTokens) -> Result<usize> {
        bail!("directed native focus regression requires Windows")
    }

    pub fn run() -> Result<()> {
        let Some(options) = Options::parse()? else {
            return Ok(());
        };
        let (output, paths) = create_data(&options.output)?;
        seed(&paths, options.dark)?;
        cliphost_core::force_headless(true);
        set_language(LanguageMode::Chinese);
        let service = NativeHost::open_with_options(NativeHostOptions {
            data_root: Some(paths.data_root.clone()),
            api_port: 0,
            headless: true,
            background: false,
            isolated: true,
            verb_rpc: false,
            core_copy_acceptance: false,
        })
        .map_err(|error| anyhow!(error))?;
        let host = service.state();
        host.remember_host_window_rect(
            "clipboard",
            60.0,
            80.0,
            f64::from(options.width),
            f64::from(options.height),
        );
        host.config_update(&json!({"showWindowLocation":"NA","enableNoFocusMode":true}))
            .map_err(|error| anyhow!(error))?;
        if options.verify_keyboard_ownership {
            host.config_update(&json!({"usingNumberKeyToPaste":true}))
                .map_err(|error| anyhow!(error))?;
        }
        host.apply_window_action_for("setPinned", true, "clipboard");
        let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
        let evidence_tokens = tokens.clone();
        let started = Instant::now();
        let live = service.clone();
        let root = output.clone();
        let port = host.api_port();
        let verify_focus = options.verify_focus;
        let verify_keyboard_ownership = options.verify_keyboard_ownership;
        let timeout_ms = if verify_focus || verify_keyboard_ownership {
            options.exit_ms.max(20_000)
        } else {
            options.exit_ms
        };
        println!(
            "{}",
            json!({"fixture":"titlebar-visual","pid":std::process::id(),"output":output,
            "headless":true,"httpListener":{"address":"127.0.0.1","port":port,"privateEphemeral":true}})
        );
        Application::new().with_assets(DesignAssets).run(move |cx| {
            let window = cx.open_window(WindowOptions {
                titlebar: None,
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(60.0), px(80.0)), size(px(options.width), px(options.height))))),
                focus: false, show: false,
                window_min_size: Some(size(px(300.0), px(250.0))), ..Default::default()
            }, move |window, cx| cx.new(|cx| {
                let mut preview = ClipboardPreview::new(tokens, UiModel::Live(live), options.dark,
                    "all".into(), String::new(), true, false, started, window, cx);
                preview.enable_diagnostics();
                preview
            })).expect("create isolated titlebar fixture");
            let route = if options.favorites { "/favorites?folderId=titlebar-a" } else { "/clipboard" };
            let mut discarded_startup_commands = vec![];
            window.update(cx, |_, window, _| {
                platform::set_no_activate(window, true).expect("no-activate titlebar fixture");
                // Public routing also queues an explicit Show. Retain pending_nav
                // for the real view, but drop startup commands synchronously so
                // its native Show path cannot raise/topmost this test window.
                host.show_web_route(route);
                while let Some(task) = host.take_chrome_task() {
                    discarded_startup_commands.push(format!("{:?}", task.command()));
                }
            }).expect("route own titlebar fixture without native Show");
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() { cx.quit(); }
            }).detach();
            cx.spawn(async move |cx| {
                let mut shown = false;
                let mut focus_checks = verify_focus.then(|| FocusScenario::new(options.favorites));
                let mut ownership_checks = verify_keyboard_ownership.then(OwnershipScenario::new);
                // Keep this guard alive across GPUI's deferred focus handoff.
                // request_search_focus inherits it; native activation/style jobs
                // are skipped, while normal entity focus and input still run.
                // Install only after initial native placement/show has settled.
                let mut focus_guard = None;
                loop {
                    Timer::after(Duration::from_millis(250)).await;
                    let self_exit = started.elapsed() >= Duration::from_millis(timeout_ms);
                    let value = window.update(cx, |view, window, cx| {
                        let pending = platform::has_pending(window).unwrap_or(true);
                        if !shown && !pending {
                            platform::set_no_activate(window, true).expect("retain no-activate fixture");
                            platform::show_no_activate(window).expect("show own fixture without activation or z-order change");
                            shown = true;
                        }
                        let viewport = window.viewport_size();
                        if verify_keyboard_ownership { window.refresh(); }
                        let main = view.report(window, cx);
                        let favorites = main["focus"]["primaryPage"] == "favorites";
                        json!({"fixture":"titlebar-visual","pid":std::process::id(),"elapsedMs":started.elapsed().as_millis(),
                            "headless":true,"isolated":true,"capture":false,"globalHooks":false,"rpc":false,"ole":false,"inputInjected":false,
                            "dataRoot":paths.data_root,"output":root,"route":route,"theme":if options.dark {"dark"} else {"light"},
                            "requestedSize":{"width":options.width,"height":options.height},
                            "viewport":{"width":f32::from(viewport.width),"height":f32::from(viewport.height),"scaleFactor":window.scale_factor()},
                            "httpListener":{"address":"127.0.0.1","port":port,"privateEphemeral":true},
                            "discardedStartupCommands":discarded_startup_commands,"nativePlacementPending":pending,"shownWithoutActivation":shown,
                            "native":native_report(window,&evidence_tokens,favorites).unwrap_or_else(|error|json!({"error":error.to_string()})),
                            "main":main,"nativeError":platform::take_error(window).ok().flatten(),"selfExit":self_exit})
                    });
                    let mut finished_focus = false;
                    let mut finished_ownership = false;
                    match value {
                        Ok(mut value) => {
                            if let Some(checks) = &mut focus_checks {
                                if self_exit {
                                    checks.timeout();
                                } else if shown && value["nativePlacementPending"] == false {
                                    if focus_guard.is_none() {
                                        focus_guard = Some(platform::guard_condition(|| false));
                                    }
                                    // SendMessage must be outside window.update:
                                    // GPUI dispatch reenters the fixture window.
                                    checks.tick(&value, &evidence_tokens);
                                }
                                finished_focus = checks.complete();
                                value["focusChecks"] = checks.report();
                                value["inputInjected"] = json!(checks.message_count > 0);
                                value["globalInputInjected"] = json!(false);
                                value["selfExit"] = json!(self_exit || finished_focus);
                            }
                            if let Some(checks) = &mut ownership_checks {
                                if self_exit {
                                    checks.timeout();
                                } else if shown && value["nativePlacementPending"] == false {
                                    if focus_guard.is_none() {
                                        focus_guard = Some(platform::guard_condition(|| false));
                                    }
                                    // Pure Core calls only, after the view borrow
                                    // ends. Unexpected paste commands are drained
                                    // synchronously before the GPUI loop resumes.
                                    checks.tick(&value, &host);
                                }
                                finished_ownership = checks.complete();
                                value["keyboardOwnershipChecks"] = checks.report();
                                value["directCoreRoutingSimulated"] = json!(true);
                                value["globalInputInjected"] = json!(false);
                                value["selfExit"] = json!(self_exit || finished_ownership);
                            }
                            if let Err(error) = serde_json::to_vec_pretty(&value)
                                .map_err(|error| anyhow!(error))
                                .and_then(|bytes|fs::write(root.join("report.json"),bytes).map_err(|error|anyhow!(error)))
                            {
                                eprintln!("fixture report failed: {error}");
                                let _ = cx.update(|cx|cx.quit());
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                    if self_exit || finished_focus || finished_ownership {
                        let _ = cx.update(|cx|cx.quit());
                        break;
                    }
                }
            }).detach();
        });
        service.shutdown();
        if verify_focus {
            let report: Value = serde_json::from_slice(&fs::read(output.join("report.json"))?)?;
            if report["focusChecks"]["passed"] != true {
                bail!(
                    "directed native focus regression failed; see {}",
                    output.join("report.json").display()
                );
            }
        }
        if verify_keyboard_ownership {
            let report: Value = serde_json::from_slice(&fs::read(output.join("report.json"))?)?;
            if report["keyboardOwnershipChecks"]["passed"] != true {
                bail!(
                    "direct Core keyboard ownership simulation failed; see {}",
                    output.join("report.json").display()
                );
            }
        }
        println!(
            "{}",
            json!({"completed":true,"output":output,"report":output.join("report.json"),"shutdown":true})
        );
        Ok(())
    }
}
