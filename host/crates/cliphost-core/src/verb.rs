//! C# `ClipVerbHostExecutor` / `PanelHostExecutor` / `SettingClipHostPages` line parser.

use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerbAction {
    None,
    Show,
    Hide,
    Toggle,
    ShowOrHide,
    Start,
    Stop,
    StartOrStop,
    Close,
    Handle,
}

impl VerbAction {
    fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "none" | "import_old_data" => Some(Self::None),
            "show" => Some(Self::Show),
            "hide" => Some(Self::Hide),
            "toggle" => Some(Self::Toggle),
            "show_or_hide" => Some(Self::ShowOrHide),
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "start_or_stop" => Some(Self::StartOrStop),
            "close" => Some(Self::Close),
            "handle" => Some(Self::Handle),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Show => "show",
            Self::Hide => "hide",
            Self::Toggle => "toggle",
            Self::ShowOrHide => "show_or_hide",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::StartOrStop => "start_or_stop",
            Self::Close => "close",
            Self::Handle => "handle",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PinChange {
    #[default]
    None,
    True,
    False,
    Toggle,
}

/// C# `CmdVerbRunContext` JSON from HTTP `/api/host/verb` and pipe `cliphost.runVerb`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerbRunContext {
    pub action_id: Option<String>,
    pub target_window_handle: i64,
    pub is_automatic_run: bool,
    /// C# `CmdVerbRunContext.TriggerType`.
    pub trigger_type: VerbTriggerType,
}

/// C# `CmdVerbTriggerType`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VerbTriggerType {
    #[default]
    Na,
    Mouse,
    Keyboard,
}

impl VerbTriggerType {
    fn from_json_value(value: Option<&Value>) -> Self {
        match value {
            Some(Value::Number(n)) => match n.as_u64().unwrap_or(0) {
                1 => Self::Mouse,
                2 => Self::Keyboard,
                _ => Self::Na,
            },
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "mouse" | "1" => Self::Mouse,
                "keyboard" | "2" => Self::Keyboard,
                _ => Self::Na,
            },
            _ => Self::Na,
        }
    }
}

impl VerbRunContext {
    pub fn from_json(value: Option<&Value>) -> Option<Self> {
        let value = value.filter(|v| !v.is_null())?;
        Some(Self {
            action_id: value
                .get("actionId")
                .or_else(|| value.get("ActionId"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty()),
            target_window_handle: value
                .get("targetWindowHandle")
                .or_else(|| value.get("TargetWindowHandle"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            is_automatic_run: value
                .get("isAutomaticRun")
                .or_else(|| value.get("IsAutomaticRun"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            trigger_type: VerbTriggerType::from_json_value(
                value
                    .get("triggerType")
                    .or_else(|| value.get("TriggerType")),
            ),
        })
    }
}

impl PinChange {
    fn parse(raw: &str) -> Self {
        match raw.to_ascii_lowercase().as_str() {
            "true" | "1" | "on" => Self::True,
            "false" | "0" | "off" => Self::False,
            "toggle" => Self::Toggle,
            _ => Self::None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::True => "true",
            Self::False => "false",
            Self::Toggle => "toggle",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostVerb {
    Empty,
    Search {
        action: VerbAction,
        source: String,
    },
    Clip {
        action: VerbAction,
        tab: String,
        focus_search: bool,
        pin: PinChange,
        fold: bool,
        loc: String,
    },
    Favor {
        action: VerbAction,
        id: String,
        all: bool,
        focus_search: bool,
    },
    FavorSync {
        action: String,
        force: bool,
    },
    Sync {
        action: String,
        slot: Option<String>,
        all: bool,
        force: bool,
        slot_refs: Vec<String>,
    },
    Collect {
        kind: String,
        title: String,
        data: String,
    },
    Panel {
        action: VerbAction,
        id: String,
    },
    Paste {
        index: i32,
        pinned_first: bool,
        next: bool,
    },
    Setting {
        page: String,
        route: String,
    },
    Menu {
        line: String,
    },
    Unknown {
        verb: String,
        line: String,
    },
}

pub fn split_command_line(line: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        if let Some(args) = command_line_to_argv_w(line) {
            return args;
        }
    }
    split_command_line_fallback(line)
}

/// C# `TextUtil.CommandLineToArgs` → `CommandLineToArgvW`.
#[cfg(windows)]
fn command_line_to_argv_w(line: &str) -> Option<Vec<String>> {
    if line.is_empty() {
        return Some(Vec::new());
    }
    unsafe {
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::CommandLineToArgvW;
        let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
        let mut argc = 0i32;
        let argv = CommandLineToArgvW(PCWSTR(wide.as_ptr()), &mut argc);
        if argv.is_null() {
            return None;
        }
        let mut out = Vec::with_capacity(argc.max(0) as usize);
        for i in 0..argc {
            let ptr = *argv.add(i as usize);
            if ptr.is_null() {
                continue;
            }
            let raw = ptr.0;
            let mut len = 0usize;
            while *raw.add(len) != 0 {
                len += 1;
            }
            out.push(String::from_utf16_lossy(std::slice::from_raw_parts(raw, len)));
        }
        let _ = local_free(argv.cast());
        Some(out)
    }
}

#[cfg(windows)]
unsafe fn local_free(hmem: *mut core::ffi::c_void) -> *mut core::ffi::c_void {
    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(hmem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }
    LocalFree(hmem)
}

fn split_command_line_fallback(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for ch in line.chars() {
        match ch {
            '"' => in_quote = !in_quote,
            c if c.is_whitespace() && !in_quote => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

pub fn parse_line(line: &str) -> Result<HostVerb, String> {
    let args = split_command_line(line.trim());
    if args.is_empty() {
        return Ok(HostVerb::Empty);
    }
    let verb = args[0].to_ascii_lowercase();
    match verb.as_str() {
        "search" | "ceabox" => parse_search(&args),
        "clip" => parse_clip(&args),
        "favor" => parse_favor(&args),
        "sync" => Ok(parse_sync(&args)),
        "collect" => parse_collect(&args),
        "panel" => parse_panel(&args),
        "paste" => parse_paste(&args),
        "setting" => parse_setting(&args),
        "menu" => Ok(HostVerb::Menu { line: line.trim().to_string() }),
        _ => Ok(HostVerb::Unknown {
            verb,
            line: line.trim().to_string(),
        }),
    }
}

fn parse_search(args: &[String]) -> Result<HostVerb, String> {
    let mut action = VerbAction::Show;
    let mut source = "all".to_string();
    let mut index = 1;
    if let Some(parsed) = args.get(index).and_then(|value| VerbAction::parse(value)) {
        if !matches!(parsed, VerbAction::Show | VerbAction::Hide | VerbAction::Toggle
            | VerbAction::ShowOrHide | VerbAction::Close | VerbAction::None) {
            return Err(format!("search verb does not support action: {}", parsed.as_str()));
        }
        action = parsed;
        index += 1;
    }
    while let Some(value) = args.get(index) {
        let scope = match value.as_str() {
            "--focus-search" => { index += 1; continue; }
            "--source" | "-t" | "--tab" => {
                index += 1;
                args.get(index).ok_or("search source is required")?.as_str()
            }
            other => other,
        };
        source = match scope.to_ascii_lowercase().as_str() {
            "all" => "all",
            "clip" | "clipboard" => "clipboard",
            "favor" | "favorite" | "favorites" => "favorite",
            "action" | "actions" => "action",
            _ => return Err(format!("unknown search source: {scope}")),
        }.into();
        index += 1;
    }
    Ok(HostVerb::Search { action, source })
}

fn parse_clip(args: &[String]) -> Result<HostVerb, String> {
    let mut action = VerbAction::None;
    let mut tab = String::new();
    let mut focus_search = false;
    let mut pin = PinChange::None;
    let mut fold = false;
    let mut loc = String::new();
    let mut i = 1;
    if i < args.len() && !args[i].starts_with('-') {
        action = VerbAction::parse(&args[i]).ok_or_else(|| format!("clip verb parse errors: {0}", args[i]))?;
        i += 1;
    }
    while i < args.len() {
        match args[i].as_str() {
            "-t" | "--tab" => {
                i += 1;
                tab = args.get(i).cloned().unwrap_or_default();
            }
            "-p" | "--pin" => {
                i += 1;
                pin = PinChange::parse(&args.get(i).cloned().unwrap_or_default());
            }
            "-s" => pin = PinChange::True,
            "-f" | "--fold" | "--fold-hide" => fold = true,
            "--loc" => {
                i += 1;
                loc = args.get(i).cloned().unwrap_or_default();
            }
            "--focus-search" => focus_search = true,
            other if other.starts_with('-') => {}
            other => {
                if tab.is_empty() {
                    tab = other.to_string();
                }
            }
        }
        i += 1;
    }
    Ok(HostVerb::Clip {
        action,
        tab,
        focus_search,
        pin,
        fold,
        loc,
    })
}

fn parse_favor(args: &[String]) -> Result<HostVerb, String> {
    if args.len() >= 2 && args[1].eq_ignore_ascii_case("sync") {
        let action = args.get(2).cloned().unwrap_or_else(|| "check".into());
        let force = args.iter().any(|a| a.eq_ignore_ascii_case("--force"));
        return Ok(HostVerb::FavorSync { action, force });
    }
    let mut action = VerbAction::None;
    let mut id = String::new();
    let mut all = false;
    let mut focus_search = false;
    let mut i = 1;
    if i < args.len() && !args[i].starts_with('-') {
        action = VerbAction::parse(&args[i]).ok_or_else(|| format!("favor verb parse errors: {0}", args[i]))?;
        i += 1;
        if i < args.len() && !args[i].starts_with('-') {
            id = args[i].clone();
            i += 1;
        }
    }
    while i < args.len() {
        match args[i].as_str() {
            "-i" | "--id" => {
                i += 1;
                id = args.get(i).cloned().unwrap_or_default();
            }
            "-a" | "--all" => all = true,
            "--focus-search" => focus_search = true,
            _ => {}
        }
        i += 1;
    }
    Ok(HostVerb::Favor {
        action,
        id,
        all,
        focus_search,
    })
}

fn parse_sync(args: &[String]) -> HostVerb {
    let action = args.get(1).cloned().unwrap_or_else(|| "check".into());
    let all = args.iter().any(|a| a.eq_ignore_ascii_case("--all"));
    let force = args.iter().any(|a| a.eq_ignore_ascii_case("--force"));
    let slot_refs: Vec<String> = args
        .iter()
        .skip(2)
        .filter(|a| !a.eq_ignore_ascii_case("--force") && !a.eq_ignore_ascii_case("--all") && !a.starts_with('-'))
        .cloned()
        .collect();
    let slot = slot_refs.first().cloned();
    HostVerb::Sync {
        action,
        slot,
        all,
        force,
        slot_refs,
    }
}

fn parse_collect(args: &[String]) -> Result<HostVerb, String> {
    let mut kind = "clip".to_string();
    let mut title = String::new();
    let mut data = String::new();
    let mut i = 1;
    if i < args.len() && !args[i].starts_with('-') {
        kind = args[i].to_ascii_lowercase();
        // C# `CollectVerbHostLine.CollectTypeEnum` CommandLine parse fails on unknown tokens.
        if !matches!(
            kind.as_str(),
            "clip" | "clipboard" | "file" | "text" | "link" | "icon"
        ) {
            return Err(format!("collect verb parse errors: {kind}"));
        }
        i += 1;
        if i < args.len() && !args[i].starts_with('-') {
            data = args[i].clone();
            i += 1;
        }
    }
    while i < args.len() {
        match args[i].as_str() {
            "-t" | "--title" => {
                i += 1;
                title = args.get(i).cloned().unwrap_or_default();
            }
            "-d" | "--data" => {
                i += 1;
                data = args.get(i).cloned().unwrap_or_default();
            }
            _ => {}
        }
        i += 1;
    }
    Ok(HostVerb::Collect { kind, title, data })
}

fn parse_panel(args: &[String]) -> Result<HostVerb, String> {
    let mut action = VerbAction::Show;
    let mut id = String::new();
    let mut i = 1;
    if i < args.len() && !args[i].starts_with('-') {
        action = match VerbAction::parse(&args[i]) {
            Some(parsed) => parsed,
            None => {
                // C# `PanelHostExecutor` CommandLine NotParsed is swallowed (no throw).
                return Ok(HostVerb::Empty);
            }
        };
        i += 1;
        if i < args.len() && !args[i].starts_with('-') {
            id = args[i].clone();
            i += 1;
        }
    }
    while i < args.len() {
        if matches!(args[i].as_str(), "-i" | "--id") {
            i += 1;
            id = args.get(i).cloned().unwrap_or_default();
        }
        i += 1;
    }
    Ok(HostVerb::Panel { action, id })
}

fn parse_paste(args: &[String]) -> Result<HostVerb, String> {
    let mut index = -1i32;
    let mut pinned_first = false;
    let mut next = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-i" | "--index" => {
                i += 1;
                index = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| "paste verb parse errors: index".to_string())?;
            }
            "-n" | "--num" => {
                i += 1;
                let number: i32 = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| "paste verb parse errors: num".to_string())?;
                index = number - 1;
            }
            "--pinned-first" | "--ui" => pinned_first = true,
            "--next" => next = true,
            "-c" | "--current" => {}
            other if !other.starts_with('-') => {
                if let Ok(value) = other.parse::<i32>() {
                    index = value;
                }
            }
            _ => {}
        }
        i += 1;
    }
    Ok(HostVerb::Paste {
        index,
        pinned_first,
        next,
    })
}

fn parse_setting(args: &[String]) -> Result<HostVerb, String> {
    let page = args.get(1).cloned().unwrap_or_default();
    // C# `SettingClipHostPages.IsClipHostOwnedPage` is false for blank pages.
    if !is_cliphost_owned_page(&page) {
        return Ok(HostVerb::Unknown {
            verb: "setting".into(),
            line: args.join(" "),
        });
    }
    Ok(HostVerb::Setting {
        route: settings_web_route(&page),
        page,
    })
}

pub fn is_cliphost_owned_page(page: &str) -> bool {
    matches!(
        normalize_settings_page(page).as_str(),
        "global"
            | "trigger"
            | "hotkeys"
            | "quickpaste"
            | "clip"
            | "clip_filter"
            | "panel"
            | "user"
            | "explorer"
            | "textaction"
            | "textactions"
            | "datalocation"
            | "about"
            | "menu"
            | "menu_global"
            | "menu_favor"
            | "menu_window"
            | "menu_file"
            // Pages of the unified main window (2026-10-23).
            | "home"
            | "lan"
            | "devices"
            | "devicesync"
            | "help"
            | "docs"
    )
}

fn normalize_settings_page(page: &str) -> String {
    page.trim().replace('.', "_").to_ascii_lowercase()
}

pub fn settings_web_route(page: &str) -> String {
    match normalize_settings_page(page).as_str() {
        "" => "/main/settings".into(),
        "global" => "/main/settings/global".into(),
        "trigger" | "hotkeys" => "/main/settings/trigger".into(),
        "quickpaste" => "/main/settings/quickPaste".into(),
        "clip" | "clip_filter" => "/main/settings/clip".into(),
        "panel" => "/main/settings/panel".into(),
        "user" | "explorer" => "/main/settings/user".into(),
        "menu" | "menu_global" | "menu_favor" | "menu_window" => "/main/settings/menu_global".into(),
        "menu_file" => "/main/settings/menu_file".into(),
        "textaction" => "/main/settings/textAction".into(),
        "textactions" => "/main/settings/textActions".into(),
        "datalocation" => "/main/settings/dataLocation".into(),
        "about" => "/main/settings/about".into(),
        "home" => "/main/home".into(),
        "lan" | "devices" | "devicesync" => "/main/settings/devices".into(),
        "help" | "docs" => "/main/docs".into(),
        _ => "/main/settings".into(),
    }
}

pub fn verb_ok(extra: Value) -> Value {
    let mut body = json!({ "ok": true });
    if let Some(map) = extra.as_object() {
        for (k, v) in map {
            body[k] = v.clone();
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_verb_opens_unified_main_window_pages() {
        for (page, route) in [("home", "/main/home"), ("LAN", "/main/settings/devices"),
            ("devices", "/main/settings/devices"), ("deviceSync", "/main/settings/devices"),
            ("textActions", "/main/settings/textActions"), ("help", "/main/docs"), ("docs", "/main/docs")] {
            assert!(is_cliphost_owned_page(page), "{page}");
            assert_eq!(settings_web_route(page), route);
        }
    }

    #[test]
    fn verb_run_context_parses_csharp_json_aliases() {
        let camel = json!({
            "actionId": "act",
            "targetWindowHandle": 42,
            "isAutomaticRun": true
        });
        let pascal = json!({
            "ActionId": "act",
            "TargetWindowHandle": 42,
            "IsAutomaticRun": true
        });
        let parsed = VerbRunContext::from_json(Some(&camel)).unwrap();
        assert_eq!(parsed, VerbRunContext::from_json(Some(&pascal)).unwrap());
        assert_eq!(parsed.target_window_handle, 42);
        assert!(parsed.is_automatic_run);
        assert_eq!(parsed.action_id.as_deref(), Some("act"));
        assert_eq!(parsed.trigger_type, VerbTriggerType::Na);
        let keyboard = VerbRunContext::from_json(Some(&json!({ "triggerType": 2 }))).unwrap();
        assert_eq!(keyboard.trigger_type, VerbTriggerType::Keyboard);
        let mouse = VerbRunContext::from_json(Some(&json!({ "TriggerType": "Mouse" }))).unwrap();
        assert_eq!(mouse.trigger_type, VerbTriggerType::Mouse);
        assert!(VerbRunContext::from_json(None).is_none());
        assert!(VerbRunContext::from_json(Some(&Value::Null)).is_none());
    }

    #[test]
    fn search_opens_aggregate_ceabox() {
        let verb = parse_line("search").unwrap();
        assert_eq!(
            verb,
            HostVerb::Search {
                action: VerbAction::Show,
                source: "all".into(),
            }
        );
    }

    #[test]
    fn ceabox_sources_and_toggle_keep_command_compatibility() {
        for (line, source) in [("search clip", "clipboard"), ("search favor", "favorite"),
            ("ceabox show --source action", "action"), ("search show -t favor --focus-search", "favorite")] {
            assert_eq!(parse_line(line).unwrap(), HostVerb::Search { action: VerbAction::Show, source: source.into() });
        }
        assert_eq!(parse_line("search show_or_hide").unwrap(), HostVerb::Search {
            action: VerbAction::ShowOrHide, source: "all".into(),
        });
        assert!(parse_line("search show --source").is_err());
        assert!(parse_line("search show --source typo").is_err());
    }

    #[test]
    fn clip_hide_and_setting_route_match_csharp() {
        assert!(matches!(
            parse_line("clip hide").unwrap(),
            HostVerb::Clip {
                action: VerbAction::Hide,
                ..
            }
        ));
        let setting = parse_line("setting textAction").unwrap();
        assert_eq!(
            setting,
            HostVerb::Setting {
                page: "textAction".into(),
                route: "/main/settings/textAction".into(),
            }
        );
        assert!(matches!(
            parse_line("setting").unwrap(),
            HostVerb::Unknown { verb, .. } if verb == "setting"
        ));
        assert!(matches!(
            parse_line("nope whatever").unwrap(),
            HostVerb::Unknown { verb, .. } if verb == "nope"
        ));
    }

    #[test]
    fn setting_data_location_opens_the_existing_settings_category() {
        for page in ["dataLocation", "DATALOCATION", "datalocation"] {
            assert!(is_cliphost_owned_page(page));
            assert_eq!(settings_web_route(page), "/main/settings/dataLocation");
            assert_eq!(parse_line(&format!("setting {page}")).unwrap(), HostVerb::Setting {
                page: page.into(), route: "/main/settings/dataLocation".into(),
            });
        }
        assert!(!is_cliphost_owned_page("dataLocationOther"));
    }

    #[test]
    fn setting_about_opens_native_updates() {
        for page in ["about", "ABOUT"] {
            assert!(is_cliphost_owned_page(page));
            assert_eq!(parse_line(&format!("setting {page}")).unwrap(), HostVerb::Setting {
                page: page.into(), route: "/main/settings/about".into(),
            });
        }
    }

    #[test]
    fn clip_parses_pin_fold_and_loc() {
        let verb = parse_line("clip show -t favor -s -f --loc m2 --focus-search").unwrap();
        assert_eq!(
            verb,
            HostVerb::Clip {
                action: VerbAction::Show,
                tab: "favor".into(),
                focus_search: true,
                pin: PinChange::True,
                fold: true,
                loc: "m2".into(),
            }
        );
        let pin = parse_line("clip show -p toggle --loc lt").unwrap();
        assert_eq!(
            pin,
            HostVerb::Clip {
                action: VerbAction::Show,
                tab: String::new(),
                focus_search: false,
                pin: PinChange::Toggle,
                fold: false,
                loc: "lt".into(),
            }
        );
    }

    #[test]
    fn sync_parses_all_force_and_orphan_refs() {
        assert_eq!(
            parse_line("sync pull --all --force").unwrap(),
            HostVerb::Sync {
                action: "pull".into(),
                slot: None,
                all: true,
                force: true,
                slot_refs: vec![],
            }
        );
        assert_eq!(
            parse_line("sync purge-orphan-favor-data cea.favor.a cea.favor.b").unwrap(),
            HostVerb::Sync {
                action: "purge-orphan-favor-data".into(),
                slot: Some("cea.favor.a".into()),
                all: false,
                force: false,
                slot_refs: vec!["cea.favor.a".into(), "cea.favor.b".into()],
            }
        );
    }

    #[test]
    fn paste_num_is_one_based() {
        let verb = parse_line("paste -n 2 --pinned-first").unwrap();
        assert_eq!(
            verb,
            HostVerb::Paste {
                index: 1,
                pinned_first: true,
                next: false,
            }
        );
    }

    #[test]
    fn collect_kind_matches_csharp_enum_parse() {
        assert!(matches!(
            parse_line("collect").unwrap(),
            HostVerb::Collect { kind, .. } if kind == "clip"
        ));
        assert!(matches!(
            parse_line("collect clipboard").unwrap(),
            HostVerb::Collect { kind, .. } if kind == "clipboard"
        ));
        let err = parse_line("collect nope").unwrap_err();
        assert!(err.contains("collect verb parse errors"), "{err}");
        // C# `PanelHostExecutor` CommandLine NotParsed is swallowed.
        assert_eq!(parse_line("panel nope").unwrap(), HostVerb::Empty);
    }

    #[test]
    fn split_command_line_uses_win32_quotes_like_csharp() {
        assert_eq!(
            super::split_command_line(r#"collect text "hello world""#),
            vec!["collect", "text", "hello world"]
        );
        assert_eq!(
            super::split_command_line(r#"clip show --loc "m 2""#),
            vec!["clip", "show", "--loc", "m 2"]
        );
        assert_eq!(super::split_command_line(""), Vec::<String>::new());
        let collect = parse_line(r#"collect text "hello world""#).unwrap();
        assert!(
            matches!(collect, HostVerb::Collect { ref data, .. } if data == "hello world"),
            "{collect:?}"
        );
    }
}
