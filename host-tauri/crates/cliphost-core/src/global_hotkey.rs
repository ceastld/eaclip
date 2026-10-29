//! C# `GlobalHotkeyChord` + `GlobalHotkeyListener` (`RegisterHotKey`).
//!
//! Quick paste stays on the low-level hook (C# `ClipHostQuickPasteService`); clip / favor /
//! Win+V / Super Menu use `RegisterHotKey` and do not swallow those chords in `WH_KEYBOARD_LL`.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::clipboard::is_headless;

pub const HOTKEY_ID_CLIP_TOGGLE: i32 = 1;
pub const HOTKEY_ID_FAVOR_TOGGLE: i32 = 2;
pub const HOTKEY_ID_WIN_V: i32 = 3;
pub const HOTKEY_ID_SUPER_MENU: i32 = 4;

pub const DEFAULT_CLIP_TOGGLE: &str = "Ctrl+Oem3";
pub const DEFAULT_FAVOR_TOGGLE: &str = "Ctrl+Shift+Oem3";
pub const DEFAULT_QUICK_PASTE: &str = "Alt+Oem3";
pub const DEFAULT_SUPER_MENU: &str = "Win+Oem3";
pub const WIN_V_CHORD: &str = "Win+V";

const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const MOD_SHIFT: u32 = 0x0004;
const MOD_WIN: u32 = 0x0008;
const MOD_NOREPEAT: u32 = 0x4000;
const ERROR_HOTKEY_ALREADY_REGISTERED: i32 = 1409;
const WM_HOTKEY: u32 = 0x0312;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HotkeyModifiers {
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub win: bool,
}

impl HotkeyModifiers {
    pub fn native_flags(self) -> u32 {
        let mut flags = 0u32;
        if self.alt {
            flags |= MOD_ALT;
        }
        if self.ctrl {
            flags |= MOD_CONTROL;
        }
        if self.shift {
            flags |= MOD_SHIFT;
        }
        if self.win {
            flags |= MOD_WIN;
        }
        flags
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingState {
    Disabled,
    Registered,
    Failed,
}

impl BindingState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "Disabled",
            Self::Registered => "Registered",
            Self::Failed => "Failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingStatus {
    pub config_key: String,
    pub label: String,
    pub chord: String,
    pub state: BindingState,
    pub message: String,
}

impl BindingStatus {
    pub fn to_json(&self) -> Value {
        json!({
            "configKey": self.config_key,
            "label": self.label,
            "chord": self.chord,
            "state": self.state.as_str(),
            "message": self.message,
        })
    }

    pub fn to_form_message(&self) -> Value {
        json!({
            "level": if self.state == BindingState::Failed { "warning" } else { "info" },
            "text": self.message,
            "fieldKey": self.config_key,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedChord {
    modifiers: HotkeyModifiers,
    canonical_key: &'static str,
    vk: u32,
}

/// C# `GlobalHotkeyChord.IsEnabled`.
pub fn is_enabled(chord: &str) -> bool {
    try_parse(chord).is_some()
}

/// C# `GlobalHotkeyChord.Normalize` — empty when unparseable.
pub fn normalize(chord: &str) -> String {
    match try_parse(chord) {
        Some(parsed) => format_canonical(parsed.modifiers, parsed.canonical_key),
        None => String::new(),
    }
}

/// C# `GlobalHotkeyChord.FormatDisplay`.
pub fn format_display(chord: &str) -> String {
    match try_parse(chord) {
        Some(parsed) => {
            let mut parts = Vec::with_capacity(5);
            if parsed.modifiers.ctrl {
                parts.push("Ctrl");
            }
            if parsed.modifiers.alt {
                parts.push("Alt");
            }
            if parsed.modifiers.shift {
                parts.push("Shift");
            }
            if parsed.modifiers.win {
                parts.push("Win");
            }
            parts.push(display_key(parsed.canonical_key));
            parts.join("+")
        }
        None => String::new(),
    }
}

/// C# `GlobalHotkeyChord.TryToNative`.
pub fn try_to_native(chord: &str) -> Option<(HotkeyModifiers, u32)> {
    let parsed = try_parse(chord)?;
    if parsed.vk == 0 {
        return None;
    }
    Some((parsed.modifiers, parsed.vk))
}

pub fn try_parse_parts(chord: &str) -> Option<(bool, bool, bool, bool, u32)> {
    let (mods, vk) = try_to_native(chord)?;
    Some((mods.ctrl, mods.alt, mods.shift, mods.win, vk))
}

fn try_parse(chord: &str) -> Option<ParsedChord> {
    let tokens: Vec<&str> = chord
        .split('+')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let mut modifiers = HotkeyModifiers::default();
    for token in &tokens[..tokens.len() - 1] {
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.ctrl = true,
            "alt" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            "win" | "windows" | "meta" | "cmd" => modifiers.win = true,
            _ => return None,
        }
    }
    let (canonical_key, vk) = parse_key_token(tokens[tokens.len() - 1])?;
    if vk == 0 {
        return None;
    }
    Some(ParsedChord {
        modifiers,
        canonical_key,
        vk,
    })
}

fn format_canonical(modifiers: HotkeyModifiers, key: &str) -> String {
    let mut parts = Vec::with_capacity(5);
    if modifiers.ctrl {
        parts.push("Ctrl");
    }
    if modifiers.alt {
        parts.push("Alt");
    }
    if modifiers.shift {
        parts.push("Shift");
    }
    if modifiers.win {
        parts.push("Win");
    }
    parts.push(key);
    parts.join("+")
}

fn display_key(canonical: &str) -> &str {
    match canonical {
        "Oem3" => "`",
        "OemMinus" => "-",
        "OemPlus" => "=",
        "OemOpenBrackets" => "[",
        "OemCloseBrackets" => "]",
        "OemPipe" => "\\",
        "OemSemicolon" => ";",
        "OemQuotes" => "'",
        "OemComma" => ",",
        "OemPeriod" => ".",
        "OemQuestion" => "/",
        "Space" => "Space",
        "D0" => "0",
        "D1" => "1",
        "D2" => "2",
        "D3" => "3",
        "D4" => "4",
        "D5" => "5",
        "D6" => "6",
        "D7" => "7",
        "D8" => "8",
        "D9" => "9",
        other if other.len() == 1 => other,
        other => other,
    }
}

fn parse_key_token(token: &str) -> Option<(&'static str, u32)> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(mapped) = alias_key(trimmed) {
        return Some(mapped);
    }
    if trimmed.len() == 1 {
        let ch = trimmed.chars().next()?;
        if ch.is_ascii_digit() {
            return named_key(&format!("D{ch}"));
        }
        if ch.is_ascii_alphabetic() {
            return named_key(&ch.to_ascii_uppercase().to_string());
        }
    }
    named_key(trimmed)
}

fn alias_key(token: &str) -> Option<(&'static str, u32)> {
    match token.to_ascii_lowercase().as_str() {
        "`" | "backquote" | "oem_3" | "tilde" | "grave" => Some(("Oem3", 0xC0)),
        "minus" => Some(("OemMinus", 0xBD)),
        "equal" => Some(("OemPlus", 0xBB)),
        "bracketleft" => Some(("OemOpenBrackets", 0xDB)),
        "bracketright" => Some(("OemCloseBrackets", 0xDD)),
        "backslash" => Some(("OemPipe", 0xDC)),
        "semicolon" => Some(("OemSemicolon", 0xBA)),
        "quote" => Some(("OemQuotes", 0xDE)),
        "comma" => Some(("OemComma", 0xBC)),
        "period" => Some(("OemPeriod", 0xBE)),
        "slash" => Some(("OemQuestion", 0xBF)),
        "space" => Some(("Space", 0x20)),
        "enter" | "return" => Some(("Return", 0x0D)),
        "escape" | "esc" => Some(("Escape", 0x1B)),
        "tab" => Some(("Tab", 0x09)),
        "backspace" => Some(("Back", 0x08)),
        "delete" | "del" => Some(("Delete", 0x2E)),
        "insert" | "ins" => Some(("Insert", 0x2D)),
        "home" => Some(("Home", 0x24)),
        "end" => Some(("End", 0x23)),
        "pageup" => Some(("PageUp", 0x21)),
        "pagedown" => Some(("PageDown", 0x22)),
        "arrowup" | "up" => Some(("Up", 0x26)),
        "arrowdown" | "down" => Some(("Down", 0x28)),
        "arrowleft" | "left" => Some(("Left", 0x25)),
        "arrowright" | "right" => Some(("Right", 0x27)),
        _ => None,
    }
}

fn named_key(token: &str) -> Option<(&'static str, u32)> {
    let lower = token.to_ascii_lowercase();
    match lower.as_str() {
        "oem3" => Some(("Oem3", 0xC0)),
        "oemminus" => Some(("OemMinus", 0xBD)),
        "oemplus" => Some(("OemPlus", 0xBB)),
        "oemopenbrackets" => Some(("OemOpenBrackets", 0xDB)),
        "oemclosebrackets" => Some(("OemCloseBrackets", 0xDD)),
        "oempipe" => Some(("OemPipe", 0xDC)),
        "oemsemicolon" => Some(("OemSemicolon", 0xBA)),
        "oemquotes" => Some(("OemQuotes", 0xDE)),
        "oemcomma" => Some(("OemComma", 0xBC)),
        "oemperiod" => Some(("OemPeriod", 0xBE)),
        "oemquestion" => Some(("OemQuestion", 0xBF)),
        "space" => Some(("Space", 0x20)),
        "return" | "enter" => Some(("Return", 0x0D)),
        "escape" => Some(("Escape", 0x1B)),
        "tab" => Some(("Tab", 0x09)),
        "back" => Some(("Back", 0x08)),
        "delete" => Some(("Delete", 0x2E)),
        "insert" => Some(("Insert", 0x2D)),
        "home" => Some(("Home", 0x24)),
        "end" => Some(("End", 0x23)),
        "pageup" => Some(("PageUp", 0x21)),
        "pagedown" => Some(("PageDown", 0x22)),
        "left" => Some(("Left", 0x25)),
        "up" => Some(("Up", 0x26)),
        "right" => Some(("Right", 0x27)),
        "down" => Some(("Down", 0x28)),
        other if other.len() == 1 && other.as_bytes()[0].is_ascii_uppercase() => {
            letter_key(other.as_bytes()[0])
        }
        other if other.len() == 1 && other.as_bytes()[0].is_ascii_lowercase() => {
            letter_key(other.as_bytes()[0].to_ascii_uppercase())
        }
        other if other.len() == 2 && other.starts_with('d') && other.as_bytes()[1].is_ascii_digit() => {
            digit_key(other.as_bytes()[1])
        }
        other if other.starts_with("numpad") && other.len() == 7 => {
            let ch = other.as_bytes()[6];
            if ch.is_ascii_digit() {
                numpad_key(ch)
            } else {
                None
            }
        }
        other if other.starts_with('f') => f_key(&other[1..]),
        _ => None,
    }
}

fn letter_key(ch: u8) -> Option<(&'static str, u32)> {
    const LETTERS: [&str; 26] = [
        "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R",
        "S", "T", "U", "V", "W", "X", "Y", "Z",
    ];
    if (b'A'..=b'Z').contains(&ch) {
        Some((LETTERS[(ch - b'A') as usize], 0x41 + (ch - b'A') as u32))
    } else {
        None
    }
}

fn digit_key(ch: u8) -> Option<(&'static str, u32)> {
    const DIGITS: [&str; 10] = ["D0", "D1", "D2", "D3", "D4", "D5", "D6", "D7", "D8", "D9"];
    if (b'0'..=b'9').contains(&ch) {
        Some((DIGITS[(ch - b'0') as usize], 0x30 + (ch - b'0') as u32))
    } else {
        None
    }
}

fn numpad_key(ch: u8) -> Option<(&'static str, u32)> {
    const KEYS: [&str; 10] = [
        "NumPad0", "NumPad1", "NumPad2", "NumPad3", "NumPad4", "NumPad5", "NumPad6", "NumPad7",
        "NumPad8", "NumPad9",
    ];
    if (b'0'..=b'9').contains(&ch) {
        Some((KEYS[(ch - b'0') as usize], 0x60 + (ch - b'0') as u32))
    } else {
        None
    }
}

fn f_key(rest: &str) -> Option<(&'static str, u32)> {
    const KEYS: [&str; 24] = [
        "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14",
        "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
    ];
    let n: u32 = rest.parse().ok()?;
    if (1..=24).contains(&n) {
        Some((KEYS[(n as usize) - 1], 0x70 + n - 1))
    } else {
        None
    }
}

/// C# `ClipHostGlobalHotkeyHostedService.ApplyBindingsCore` without the HWND.
pub fn plan_bindings(
    clip_toggle: &str,
    favor_toggle: &str,
    quick_paste: &str,
    super_menu: &str,
    use_win_v: bool,
    listener_ready: bool,
) -> Vec<BindingStatus> {
    let mut seen = Vec::<String>::new();
    let mut statuses = Vec::with_capacity(5);
    statuses.push(plan_register_binding(
        HOTKEY_ID_CLIP_TOGGLE,
        "HotkeyClipToggle",
        "显示/隐藏剪贴板窗口",
        clip_toggle,
        &mut seen,
        listener_ready,
    ));
    statuses.push(plan_register_binding(
        HOTKEY_ID_FAVOR_TOGGLE,
        "HotkeyFavorToggle",
        "收藏面板",
        favor_toggle,
        &mut seen,
        listener_ready,
    ));
    statuses.push(plan_quick_paste_binding(quick_paste, &mut seen));
    if use_win_v {
        statuses.push(plan_register_binding(
            HOTKEY_ID_WIN_V,
            "UseWinVShortcut",
            "Win+V 打开剪贴板",
            WIN_V_CHORD,
            &mut seen,
            listener_ready,
        ));
    } else {
        statuses.push(BindingStatus {
            config_key: "UseWinVShortcut".into(),
            label: "Win+V 打开剪贴板".into(),
            chord: String::new(),
            state: BindingState::Disabled,
            message: "Win+V 打开剪贴板已关闭。".into(),
        });
    }
    // Keep existing bindings first so adding Super Menu cannot disable an
    // established clipboard, favorites, quick paste or Win+V binding.
    statuses.push(plan_register_binding(
        HOTKEY_ID_SUPER_MENU,
        "HotkeySuperMenu",
        "超级菜单",
        super_menu,
        &mut seen,
        listener_ready,
    ));
    statuses
}

fn plan_register_binding(
    _id: i32,
    config_key: &str,
    label: &str,
    chord: &str,
    seen: &mut Vec<String>,
    listener_ready: bool,
) -> BindingStatus {
    let normalized = normalize(chord);
    if normalized.is_empty() {
        return BindingStatus {
            config_key: config_key.into(),
            label: label.into(),
            chord: String::new(),
            state: BindingState::Disabled,
            message: format!("{label} 全局快捷键已禁用。"),
        };
    }
    let display = format_display(&normalized);
    if seen
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(&normalized))
    {
        return BindingStatus {
            config_key: config_key.into(),
            label: label.into(),
            chord: normalized,
            state: BindingState::Failed,
            message: format!("{label} 快捷键 {display} 与其他命令重复，未注册。"),
        };
    }
    seen.push(normalized.clone());
    if !listener_ready {
        return BindingStatus {
            config_key: config_key.into(),
            label: label.into(),
            chord: normalized,
            state: BindingState::Failed,
            message: format!("{label} 快捷键 {display} 注册失败：监听器未初始化。"),
        };
    }
    BindingStatus {
        config_key: config_key.into(),
        label: label.into(),
        chord: normalized,
        state: BindingState::Registered,
        message: format!("{label} 快捷键已注册为 {display}。"),
    }
}

fn plan_quick_paste_binding(chord: &str, seen: &mut Vec<String>) -> BindingStatus {
    let normalized = normalize(chord);
    if normalized.is_empty() {
        return BindingStatus {
            config_key: "HotkeyQuickPaste".into(),
            label: "便捷粘贴".into(),
            chord: String::new(),
            state: BindingState::Disabled,
            message: "便捷粘贴全局快捷键已禁用。".into(),
        };
    }
    let display = format_display(&normalized);
    if seen
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(&normalized))
    {
        return BindingStatus {
            config_key: "HotkeyQuickPaste".into(),
            label: "便捷粘贴".into(),
            chord: normalized,
            state: BindingState::Failed,
            message: format!("便捷粘贴快捷键 {display} 与其他命令重复，未启用。"),
        };
    }
    seen.push(normalized.clone());
    if try_to_native(&normalized).is_none() {
        return BindingStatus {
            config_key: "HotkeyQuickPaste".into(),
            label: "便捷粘贴".into(),
            chord: normalized,
            state: BindingState::Failed,
            message: format!("便捷粘贴快捷键 {display} 无法识别，未启用。"),
        };
    }
    BindingStatus {
        config_key: "HotkeyQuickPaste".into(),
        label: "便捷粘贴".into(),
        chord: normalized,
        state: BindingState::Registered,
        message: "便捷粘贴已启用（按住 Alt，` 下一条，Q 上一条，1/2 选操作，D 删除，E 粘贴并回车，F 连续粘贴，松开 Alt 粘贴）。".into(),
    }
}

pub fn apply_win32_results(statuses: &mut [BindingStatus], results: &[(i32, i32)]) {
    for status in statuses {
        if status.state != BindingState::Registered {
            continue;
        }
        let Some(id) = native_id_for(&status.config_key) else {
            continue;
        };
        // A planned binding is only registered after the listener confirms it.
        // Missing results also cover a disconnected listener or a timed-out reply.
        let win32 = results
            .iter()
            .find(|(result_id, _)| *result_id == id)
            .map(|(_, code)| *code);
        if win32 == Some(0) {
            continue;
        }
        let display = format_display(&status.chord);
        status.state = BindingState::Failed;
        status.message = match win32 {
            Some(ERROR_HOTKEY_ALREADY_REGISTERED) => {
                format!("{} 快捷键 {display} 已被其他程序占用，注册失败。", status.label)
            }
            Some(code) => format!(
                "{} 快捷键 {display} 注册失败（Win32={code}）。",
                status.label
            ),
            None => format!(
                "{} 快捷键 {display} 注册未确认：未收到注册结果。",
                status.label
            ),
        };
    }
}

fn native_id_for(config_key: &str) -> Option<i32> {
    match config_key {
        "HotkeyClipToggle" => Some(HOTKEY_ID_CLIP_TOGGLE),
        "HotkeyFavorToggle" => Some(HOTKEY_ID_FAVOR_TOGGLE),
        "UseWinVShortcut" => Some(HOTKEY_ID_WIN_V),
        "HotkeySuperMenu" => Some(HOTKEY_ID_SUPER_MENU),
        _ => None,
    }
}

pub fn natives_to_register(statuses: &[BindingStatus]) -> Vec<(i32, u32, u32)> {
    statuses
        .iter()
        .filter(|s| s.state == BindingState::Registered)
        .filter_map(|s| {
            let id = native_id_for(&s.config_key)?;
            let (mods, vk) = try_to_native(&s.chord)?;
            // One key press opens one menu session, even when the key is held.
            // Preserve the existing repeat behavior of every other binding.
            let flags = mods.native_flags()
                | if id == HOTKEY_ID_SUPER_MENU { MOD_NOREPEAT } else { 0 };
            Some((id, flags, vk))
        })
        .collect()
}

/// C# `SettingsFormFacade.BuildGlobalHotkeyMessages`.
pub fn form_messages(statuses: &[BindingStatus], changed_keys: &[String]) -> Vec<Value> {
    if statuses.is_empty() || changed_keys.is_empty() {
        return Vec::new();
    }
    let changed = |status: &BindingStatus| {
        changed_keys.iter().any(|key| key.eq_ignore_ascii_case(&status.config_key))
    };
    let mut relevant = statuses
        .iter()
        .filter(|status| status.state == BindingState::Failed || changed(status))
        .collect::<Vec<_>>();
    // The settings surface shows the first message. Keep the current edit's
    // result visible before unrelated failures, preserving order within groups.
    relevant.sort_by_key(|status| match (changed(status), status.state == BindingState::Failed) {
        (true, true) => 0,
        (true, false) => 1,
        _ => 2,
    });
    relevant
        .into_iter()
        .map(BindingStatus::to_form_message)
        .collect()
}

pub fn changed_config_keys(
    prev_clip: &str,
    next_clip: &str,
    prev_favor: &str,
    next_favor: &str,
    prev_qp: &str,
    next_qp: &str,
    prev_super_menu: &str,
    next_super_menu: &str,
    previous_win_v: bool,
    next_win_v: bool,
    form_id: &str,
) -> Vec<String> {
    let mut keys = Vec::new();
    if form_id == "quickPaste" && normalize(prev_qp) != normalize(next_qp) {
        keys.push("HotkeyQuickPaste".into());
    }
    if form_id == "trigger" || form_id == "hotkeys" {
        if normalize(prev_clip) != normalize(next_clip) {
            keys.push("HotkeyClipToggle".into());
        }
        if normalize(prev_favor) != normalize(next_favor) {
            keys.push("HotkeyFavorToggle".into());
        }
        if normalize(prev_super_menu) != normalize(next_super_menu) {
            keys.push("HotkeySuperMenu".into());
        }
        if previous_win_v != next_win_v {
            keys.push("UseWinVShortcut".into());
        }
    }
    keys
}

pub fn quick_paste_enabled(statuses: &[BindingStatus]) -> bool {
    statuses.iter().any(|s| {
        s.config_key == "HotkeyQuickPaste" && s.state == BindingState::Registered
    })
}

pub fn verb_line_for_id(id: i32) -> Option<&'static str> {
    match id {
        HOTKEY_ID_CLIP_TOGGLE | HOTKEY_ID_WIN_V => Some("clip toggle"),
        HOTKEY_ID_FAVOR_TOGGLE => Some("favor show_or_hide"),
        HOTKEY_ID_SUPER_MENU => Some("menu super"),
        _ => None,
    }
}

enum ListenerCmd {
    Replace {
        bindings: Vec<(i32, u32, u32)>,
        reply: Sender<Vec<(i32, i32)>>,
    },
    Shutdown,
}

pub struct GlobalHotkeyListener {
    tx: Sender<ListenerCmd>,
    #[cfg(windows)]
    wake: Arc<CommandSignal>,
}

#[cfg(windows)]
struct CommandSignal(isize);

#[cfg(windows)]
impl CommandSignal {
    fn new() -> windows::core::Result<Self> {
        unsafe {
            windows::Win32::System::Threading::CreateEventW(None, false, false, None)
                .map(|event| Self(event.0 as isize))
        }
    }

    fn signal(&self) {
        unsafe {
            if let Err(error) = windows::Win32::System::Threading::SetEvent(self.handle()) {
                tracing::warn!(%error, "Could not wake the global hotkey listener");
            }
        }
    }

    fn handle(&self) -> windows::Win32::Foundation::HANDLE {
        windows::Win32::Foundation::HANDLE(self.0 as *mut _)
    }
}

#[cfg(windows)]
impl Drop for CommandSignal {
    fn drop(&mut self) {
        unsafe { let _ = windows::Win32::Foundation::CloseHandle(self.handle()); }
    }
}

impl GlobalHotkeyListener {
    pub fn start(on_hotkey: Arc<dyn Fn(i32) + Send + Sync>) -> Option<Self> {
        if is_headless() {
            return None;
        }
        #[cfg(not(windows))]
        {
            let _ = on_hotkey;
            return None;
        }
        #[cfg(windows)]
        {
            let (tx, rx) = mpsc::channel();
            let wake = match CommandSignal::new() {
                Ok(wake) => Arc::new(wake),
                Err(error) => {
                    tracing::warn!(%error, "Could not create the global hotkey command event");
                    return None;
                }
            };
            // Create the event before spawning: replacements/shutdown queued
            // during HWND initialization retain their wakeup.
            let worker_wake = wake.clone();
            match thread::Builder::new()
                .name("cliphost-global-hotkey".into())
                .spawn(move || unsafe { pump(rx, worker_wake, on_hotkey) })
            {
                Ok(_) => Some(Self { tx, wake }),
                Err(error) => {
                    tracing::warn!(%error, "Could not start the global hotkey listener");
                    None
                }
            }
        }
    }

    pub fn replace_bindings(&self, bindings: Vec<(i32, u32, u32)>) -> Vec<(i32, i32)> {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self
            .tx
            .send(ListenerCmd::Replace {
                bindings,
                reply: reply_tx,
            })
            .is_err()
        {
            return Vec::new();
        }
        #[cfg(windows)]
        self.wake.signal();
        reply_rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default()
    }
}

impl Drop for GlobalHotkeyListener {
    fn drop(&mut self) {
        let _ = self.tx.send(ListenerCmd::Shutdown);
        #[cfg(windows)]
        self.wake.signal();
    }
}

#[cfg(windows)]
unsafe fn pump(rx: Receiver<ListenerCmd>, wake: Arc<CommandSignal>, on_hotkey: Arc<dyn Fn(i32) + Send + Sync>) {
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM, WAIT_FAILED};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::INFINITE;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
        RegisterClassW, TranslateMessage, HWND_MESSAGE, MSG, PM_REMOVE, WINDOW_EX_STYLE,
        WINDOW_STYLE, WM_DESTROY, WM_QUIT, WNDCLASSW, MsgWaitForMultipleObjectsEx,
        QS_ALLINPUT, MWMO_INPUTAVAILABLE,
    };

    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == WM_DESTROY {
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    let class = w!("ClipHostTauriGlobalHotkey");
    let Ok(hinst) = GetModuleHandleW(None) else {
        return;
    };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst.into(),
        lpszClassName: class,
        ..Default::default()
    };
    let _ = RegisterClassW(&wc);
    let hwnd = match CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class,
        w!("ClipHostTauriGlobalHotkey"),
        WINDOW_STYLE::default(),
        0,
        0,
        0,
        0,
        HWND_MESSAGE,
        None,
        hinst,
        None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };
    let mut registered: Vec<i32> = Vec::new();
    let mut msg = MSG::default();
    'messages: loop {
        loop {
            let cmd = match rx.try_recv() {
                Ok(cmd) => cmd,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break 'messages,
            };
            match cmd {
                ListenerCmd::Shutdown => break 'messages,
                ListenerCmd::Replace { bindings, reply } => {
                    unregister_all(hwnd, &mut registered);
                    let mut results = Vec::new();
                    for (id, mods, vk) in bindings {
                        let err = register_one(hwnd, id, mods, vk);
                        if err == 0 {
                            registered.push(id);
                        }
                        results.push((id, err));
                    }
                    let _ = reply.send(results);
                }
            }
        }
        if PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT { break; }
            if msg.message == WM_HOTKEY {
                on_hotkey(msg.wParam.0 as i32);
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            continue;
        }
        // Both command arrival and WM_HOTKEY wake this wait immediately. The
        // auto-reset event coalesces commands; the channel retains every one.
        if MsgWaitForMultipleObjectsEx(Some(&[wake.handle()]), INFINITE,
            QS_ALLINPUT, MWMO_INPUTAVAILABLE) == WAIT_FAILED {
            tracing::warn!(error = %windows::core::Error::from_win32(), "Global hotkey message wait failed");
            break;
        }
    }
    unregister_all(hwnd, &mut registered);
    let _ = DestroyWindow(hwnd);
}

#[cfg(windows)]
unsafe fn register_one(hwnd: windows::Win32::Foundation::HWND, id: i32, mods: u32, vk: u32) -> i32 {
    use windows::Win32::Foundation::GetLastError;
    use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, HOT_KEY_MODIFIERS};

    if RegisterHotKey(hwnd, id, HOT_KEY_MODIFIERS(mods), vk).is_ok() {
        0
    } else {
        GetLastError().0 as i32
    }
}

#[cfg(windows)]
unsafe fn unregister_all(hwnd: windows::Win32::Foundation::HWND, registered: &mut Vec<i32>) {
    use windows::Win32::UI::Input::KeyboardAndMouse::UnregisterHotKey;

    for id in registered.drain(..) {
        let _ = UnregisterHotKey(hwnd, id);
    }
}

#[cfg(all(test, windows))]
#[path = "global_hotkey_native_tests.rs"]
mod native_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_listener_does_not_report_registration_success() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let listener = GlobalHotkeyListener { tx, #[cfg(windows)] wake: Arc::new(CommandSignal::new().unwrap()) };
        let mut statuses = plan_bindings(
            DEFAULT_CLIP_TOGGLE, DEFAULT_FAVOR_TOGGLE, DEFAULT_QUICK_PASTE, "Ctrl+Alt+M", true, true,
        );
        let results = listener.replace_bindings(natives_to_register(&statuses));
        assert!(results.is_empty());
        apply_win32_results(&mut statuses, &results);
        for status in statuses.iter().filter(|s| native_id_for(&s.config_key).is_some()) {
            assert_eq!(status.state, BindingState::Failed, "{}", status.config_key);
            assert!(status.message.contains("未收到注册结果"));
        }
        assert!(quick_paste_enabled(&statuses), "The low-level hook is independent");
        let messages = form_messages(&statuses, &["HotkeyClipToggle".into()]);
        assert_eq!(messages.iter().filter(|m| m["level"] == "warning").count(), 4);
    }

    #[test]
    fn partial_registration_results_require_confirmation_for_each_hotkey() {
        let mut statuses = plan_bindings(
            DEFAULT_CLIP_TOGGLE, DEFAULT_FAVOR_TOGGLE, DEFAULT_QUICK_PASTE, "", true, true,
        );
        apply_win32_results(&mut statuses, &[
            (HOTKEY_ID_CLIP_TOGGLE, 0),
            (HOTKEY_ID_FAVOR_TOGGLE, ERROR_HOTKEY_ALREADY_REGISTERED),
        ]);
        assert_eq!(statuses[0].state, BindingState::Registered);
        assert_eq!(statuses[1].state, BindingState::Failed);
        assert!(statuses[1].message.contains("已被其他程序占用"));
        assert_eq!(statuses[2].state, BindingState::Registered);
        assert_eq!(statuses[3].state, BindingState::Failed);
        assert!(statuses[3].message.contains("未收到注册结果"));
    }

    #[test]
    fn complete_registration_results_preserve_disabled_and_duplicate_bindings() {
        let mut statuses = plan_bindings("", WIN_V_CHORD, DEFAULT_QUICK_PASTE, "", true, true);
        let disabled_message = statuses[0].message.clone();
        let duplicate_message = statuses[3].message.clone();
        apply_win32_results(&mut statuses, &[(HOTKEY_ID_FAVOR_TOGGLE, 0)]);
        assert_eq!(statuses[0].state, BindingState::Disabled);
        assert_eq!(statuses[0].message, disabled_message);
        assert_eq!(statuses[1].state, BindingState::Registered);
        assert_eq!(statuses[2].state, BindingState::Registered);
        assert_eq!(statuses[3].state, BindingState::Failed);
        assert_eq!(statuses[3].message, duplicate_message);
    }

    #[test]
    fn normalize_preserves_separator_between_modifiers_and_key() {
        let normalized = normalize(DEFAULT_CLIP_TOGGLE);
        assert_eq!(normalized, "Ctrl+Oem3");
        assert_eq!(format_display(&normalized), "Ctrl+`");
    }

    #[test]
    fn try_to_native_accepts_normalized_default_hotkeys() {
        let clip = normalize(DEFAULT_CLIP_TOGGLE);
        let favor = normalize(DEFAULT_FAVOR_TOGGLE);
        let (clip_mods, clip_vk) = try_to_native(&clip).unwrap();
        assert_eq!(clip_mods.native_flags(), MOD_CONTROL);
        assert_eq!(clip_vk, 0xC0);
        let (favor_mods, favor_vk) = try_to_native(&favor).unwrap();
        assert_eq!(favor_mods.native_flags(), MOD_CONTROL | MOD_SHIFT);
        assert_eq!(favor_vk, 0xC0);
        let (qp_mods, qp_vk) = try_to_native(DEFAULT_QUICK_PASTE).unwrap();
        assert!(qp_mods.alt && !qp_mods.ctrl);
        assert_eq!(qp_vk, 0xC0);
        let (win_mods, win_vk) = try_to_native(WIN_V_CHORD).unwrap();
        assert!(win_mods.win && !win_mods.ctrl && !win_mods.alt && !win_mods.shift);
        assert_eq!(win_vk, 0x56);
    }

    #[test]
    fn display_accepts_backtick_alias() {
        assert_eq!(normalize("Alt+`"), "Alt+Oem3");
        assert_eq!(format_display("Alt+Oem3"), "Alt+`");
    }

    #[test]
    fn plan_bindings_defaults_are_unique() {
        let statuses = plan_bindings(
            DEFAULT_CLIP_TOGGLE,
            DEFAULT_FAVOR_TOGGLE,
            DEFAULT_QUICK_PASTE,
            "",
            false,
            false,
        );
        assert_eq!(statuses[0].config_key, "HotkeyClipToggle");
        assert_eq!(statuses[0].chord, "Ctrl+Oem3");
        assert_eq!(statuses[0].state, BindingState::Failed);
        assert!(statuses[0].message.contains("监听器未初始化"));
        assert_eq!(statuses[1].chord, "Ctrl+Shift+Oem3");
        assert_eq!(statuses[2].state, BindingState::Registered);
        assert_eq!(statuses[2].config_key, "HotkeyQuickPaste");
        assert_eq!(statuses[3].state, BindingState::Disabled);
    }

    #[test]
    fn duplicate_clip_and_quick_paste_fails_quick_paste() {
        let statuses = plan_bindings("Alt+Oem3", DEFAULT_FAVOR_TOGGLE, "Alt+`", "", false, true);
        assert_eq!(statuses[0].state, BindingState::Registered);
        assert_eq!(statuses[2].state, BindingState::Failed);
        assert!(statuses[2].message.contains("与其他命令重复"));
        assert!(!quick_paste_enabled(&statuses));
    }

    #[test]
    fn empty_chord_disables_binding() {
        let statuses = plan_bindings("", DEFAULT_FAVOR_TOGGLE, "   ", "", false, true);
        assert_eq!(statuses[0].state, BindingState::Disabled);
        assert_eq!(statuses[2].state, BindingState::Disabled);
        assert_eq!(statuses[2].message, "便捷粘贴全局快捷键已禁用。");
    }

    #[test]
    fn form_messages_include_failures_when_any_key_changed() {
        let statuses = plan_bindings(
            DEFAULT_CLIP_TOGGLE,
            DEFAULT_FAVOR_TOGGLE,
            DEFAULT_QUICK_PASTE,
            "",
            false,
            false,
        );
        assert!(form_messages(&statuses, &[]).is_empty());
        let msgs = form_messages(&statuses, &["HotkeyClipToggle".into()]);
        assert!(msgs.iter().any(|m| m["fieldKey"] == "HotkeyClipToggle"));
        assert!(msgs.iter().any(|m| m["level"] == "warning"));
    }

    #[test]
    fn super_menu_binding_registers_its_own_native_command() {
        let mut statuses = plan_bindings(
            DEFAULT_CLIP_TOGGLE,
            DEFAULT_FAVOR_TOGGLE,
            DEFAULT_QUICK_PASTE,
            DEFAULT_SUPER_MENU,
            true,
            true,
        );
        assert_eq!(statuses[4].config_key, "HotkeySuperMenu");
        assert_eq!(statuses[4].state, BindingState::Registered);
        assert_eq!(statuses[4].chord, "Win+Oem3");
        let natives = natives_to_register(&statuses);
        assert!(natives.contains(&(
            HOTKEY_ID_SUPER_MENU,
            MOD_WIN | MOD_NOREPEAT,
            0xC0,
        )));
        assert!(natives.iter().filter(|(id, _, _)| *id != HOTKEY_ID_SUPER_MENU)
            .all(|(_, flags, _)| flags & MOD_NOREPEAT == 0));
        assert_eq!(verb_line_for_id(HOTKEY_ID_SUPER_MENU), Some("menu super"));
        apply_win32_results(&mut statuses, &[
            (HOTKEY_ID_CLIP_TOGGLE, 0),
            (HOTKEY_ID_FAVOR_TOGGLE, 0),
            (HOTKEY_ID_WIN_V, 0),
            (HOTKEY_ID_SUPER_MENU, 0),
        ]);
        assert_eq!(statuses[4].state, BindingState::Registered);
        let messages = form_messages(&statuses, &["HotkeySuperMenu".into()]);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["fieldKey"], "HotkeySuperMenu");
        assert_eq!(messages[0]["level"], "info");
    }

    #[test]
    fn super_menu_duplicates_preserve_every_existing_binding() {
        for chord in [DEFAULT_CLIP_TOGGLE, DEFAULT_FAVOR_TOGGLE, "Alt+`", WIN_V_CHORD] {
            let original = plan_bindings(
                DEFAULT_CLIP_TOGGLE, DEFAULT_FAVOR_TOGGLE, DEFAULT_QUICK_PASTE, "", true, true,
            );
            let statuses = plan_bindings(
                DEFAULT_CLIP_TOGGLE, DEFAULT_FAVOR_TOGGLE, DEFAULT_QUICK_PASTE, chord, true, true,
            );
            assert_eq!(&statuses[..4], &original[..4], "{chord}");
            assert_eq!(statuses[4].state, BindingState::Failed, "{chord}");
            assert!(statuses[4].message.contains("与其他命令重复"));
            assert!(!natives_to_register(&statuses).iter().any(|(id, _, _)| *id == HOTKEY_ID_SUPER_MENU));
            assert!(quick_paste_enabled(&statuses));
        }
    }

    #[test]
    fn super_menu_disabled_or_failed_registration_is_reported_by_field() {
        let disabled = plan_bindings("", "", "", "", false, true);
        assert_eq!(disabled[4].state, BindingState::Disabled);
        assert!(natives_to_register(&disabled).is_empty());

        let unavailable = plan_bindings("", "", "", "Ctrl+Alt+M", false, false);
        assert_eq!(unavailable[4].state, BindingState::Failed);
        assert!(unavailable[4].message.contains("监听器未初始化"));

        for (results, expected) in [
            (vec![(HOTKEY_ID_SUPER_MENU, ERROR_HOTKEY_ALREADY_REGISTERED)], "已被其他程序占用"),
            (vec![], "未收到注册结果"),
        ] {
            let mut statuses = plan_bindings("", "", "", "Ctrl+Alt+M", false, true);
            apply_win32_results(&mut statuses, &results);
            assert_eq!(statuses[4].state, BindingState::Failed);
            assert!(statuses[4].message.contains(expected));
            let messages = form_messages(&statuses, &["HotkeySuperMenu".into()]);
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0]["fieldKey"], "HotkeySuperMenu");
            assert_eq!(messages[0]["level"], "warning");
        }
    }

    #[test]
    fn super_menu_changes_report_only_canonical_trigger_updates() {
        for form in ["trigger", "hotkeys"] {
            let keys = changed_config_keys(
                "", "", "", "", "", "", "Ctrl+Alt+M", "Ctrl+Alt+N", false, false, form,
            );
            assert_eq!(keys, vec!["HotkeySuperMenu"]);
            let cleared = changed_config_keys(
                "", "", "", "", "", "", "Ctrl+Alt+M", "", false, false, form,
            );
            assert_eq!(cleared, vec!["HotkeySuperMenu"]);
            let unchanged = changed_config_keys(
                "", "", "", "", "", "", "Ctrl+Alt+`", "Ctrl+Alt+Oem3", false, false, form,
            );
            assert!(unchanged.is_empty());
        }
        assert!(changed_config_keys(
            "", "", "", "", "", "", "", "Ctrl+Alt+M", false, false, "quickPaste",
        ).is_empty());
    }

    #[test]
    fn current_super_menu_results_precede_unrelated_hotkey_failures() {
        for (chord, expected_level) in [
            (DEFAULT_CLIP_TOGGLE, "warning"),
            (DEFAULT_SUPER_MENU, "info"),
            ("", "info"),
        ] {
            let mut statuses = plan_bindings(
                DEFAULT_CLIP_TOGGLE,
                DEFAULT_FAVOR_TOGGLE,
                DEFAULT_QUICK_PASTE,
                chord,
                false,
                true,
            );
            apply_win32_results(&mut statuses, &[
                (HOTKEY_ID_CLIP_TOGGLE, ERROR_HOTKEY_ALREADY_REGISTERED),
                (HOTKEY_ID_FAVOR_TOGGLE, 0),
                (HOTKEY_ID_SUPER_MENU, 0),
            ]);
            let messages = form_messages(&statuses, &["hotkeysupermenu".into()]);
            assert_eq!(messages.len(), 2, "{chord}");
            assert_eq!(messages[0]["fieldKey"], "HotkeySuperMenu", "{chord}");
            assert_eq!(messages[0]["level"], expected_level, "{chord}");
            assert_eq!(messages[1]["fieldKey"], "HotkeyClipToggle", "{chord}");

            if chord == DEFAULT_CLIP_TOGGLE {
                let messages = form_messages(&statuses, &[
                    "HotkeyFavorToggle".into(),
                    "HotkeySuperMenu".into(),
                    "HotkeySuperMenu".into(),
                ]);
                let fields = messages.iter().map(|message| message["fieldKey"].as_str().unwrap())
                    .collect::<Vec<_>>();
                assert_eq!(fields, ["HotkeySuperMenu", "HotkeyFavorToggle", "HotkeyClipToggle"]);
            }
        }
    }
}
