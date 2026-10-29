use crate::clipboard::{foreground_target, write_record};
use crate::model::ClipRecord;
use crate::input_target::InputTarget;
use crate::text_actions::apply as apply_text_action;

#[cfg(windows)]
// Shared by every generated event and preserved by partial-send key releases.
pub(crate) const HOST_KEYBOARD_INPUT_TAG: usize = 0x4348_5253;

#[derive(Debug, Clone, Default)]
pub struct PasteOptions {
    pub delay_ms: i32,
    pub extra_shift_insert: String,
    pub paste_with_new_line: bool,
    pub standard_paste: bool,
    pub plain_text: bool,
    pub expected_target: Option<InputTarget>,
    pub expected_clipboard_sequence: Option<u32>,
}

pub fn copy_record(item: &ClipRecord) -> Result<(), String> {
    write_record(item, false, false, false)
}

pub fn copy_record_with(item: &ClipRecord, opts: &PasteOptions) -> Result<(), String> {
    prepare_copy_record(item, opts)?.write()
}

pub(crate) fn prepare_copy_record(item: &ClipRecord, opts: &PasteOptions) -> Result<crate::clipboard_payload::PreparedClipboard, String> {
    crate::clipboard_payload::require_output_projection(item)?;
    crate::clipboard_payload::prepare_record(item, opts.plain_text, opts.paste_with_new_line, opts.standard_paste)
}

pub fn paste_record(item: &ClipRecord) -> Result<(), String> {
    paste_record_with(item, &PasteOptions::default())
}

pub fn paste_record_with(item: &ClipRecord, opts: &PasteOptions) -> Result<(), String> {
    let mut opts = opts.clone();
    let target = resolve_input_target(opts.expected_target)?;
    target.verify_for_paste()?;
    opts.expected_target = Some(target);
    copy_record_with(item, &opts)?;
    if !crate::clipboard::is_headless() {
        opts.expected_clipboard_sequence = Some(crate::clipboard::sequence_number());
    }
    send_paste(&opts)
}

/// C# `PasteKeyDispatch.SendPaste` delay after `PasteActionStep`.
/// Non-QP floors negative to 50ms; `SuppressPasteDelay` (QP) keeps 0. C# `WinProperty.IsWeChat` still `max(100)`.
pub fn resolve_paste_delay_ms(delay_ms: i32, process_name: &str) -> i32 {
    let mut delay = delay_ms;
    if delay < 0 {
        delay = 50;
    }
    if strip_exe(process_name).eq_ignore_ascii_case("wechat") {
        delay = delay.max(100);
    }
    delay
}

/// C# `ClipHostQuickPasteService.PasteEnterDelay` (30ms) before `{ENTER}` after paste.
pub const QUICK_PASTE_ENTER_DELAY_MS: u64 = 30;

const MODIFIER_RELEASE_TIMEOUT_MS: u64 = 2000;

/// A release of one chord modifier must not turn Ctrl+V into Alt/Shift/Win+Ctrl+V.
/// Wait on the input worker without releasing keys the user still holds.
pub(crate) fn wait_for_modifier_release_to(target: InputTarget) -> Result<(), String> {
    wait_for_modifier_release_to_with_guard(target, || Ok(()))
}

/// Quick paste: a newer chord retires the pending paste through `guard`. Check
/// it while waiting, or the serial input worker stays blocked on the new
/// chord's held modifier and its BeginSession only runs after release.
pub(crate) fn wait_for_modifier_release_to_with_guard(
    target: InputTarget,
    mut guard: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    if crate::clipboard::is_headless() { return Ok(()); }
    let started = std::time::Instant::now();
    wait_for_modifier_release_checked(
        || { guard()?; target.verify_for_paste() },
        || {
            #[cfg(windows)]
            unsafe {
                use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
                [0x10, 0x11, 0x12, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C]
                    .into_iter().any(|vk| GetAsyncKeyState(vk) < 0)
            }
            #[cfg(not(windows))]
            { false }
        },
        || started.elapsed().as_millis() as u64,
        sleep_input_delay,
    )
}

fn wait_for_modifier_release_checked(
    mut verify: impl FnMut() -> Result<(), String>,
    mut modifiers_down: impl FnMut() -> bool,
    mut elapsed_ms: impl FnMut() -> u64,
    mut wait: impl FnMut(u64),
) -> Result<(), String> {
    loop {
        verify()?;
        if !modifiers_down() { return Ok(()); }
        let elapsed = elapsed_ms();
        if elapsed >= MODIFIER_RELEASE_TIMEOUT_MS {
            return Err("修饰键仍按住，已停止本次便捷粘贴；请松开后重试。".into());
        }
        wait(10.min(MODIFIER_RELEASE_TIMEOUT_MS - elapsed));
    }
}

/// C# `ClipHostToastOverlayWebViewWindow.PasteEnterDelay` (40ms).
pub const TOAST_PASTE_ENTER_DELAY_MS: u64 = 40;

pub fn send_enter_after(delay_ms: u64) -> Result<(), String> {
    send_enter_after_to(delay_ms, None)
}

pub fn send_enter_after_to(delay_ms: u64, target: Option<InputTarget>) -> Result<(), String> {
    send_enter_after_to_with_guard(delay_ms, target, || Ok(()))
}

pub(crate) fn send_enter_after_to_with_guard(delay_ms: u64, target: Option<InputTarget>, mut verify: impl FnMut() -> Result<(), String>) -> Result<(), String> {
    verify()?;
    if crate::clipboard::is_headless() { return Ok(()); }
    let target = resolve_input_target(target)?;
    dispatch_checked_input(delay_ms, || { verify()?; target.verify_for_paste() }, sleep_input_delay, send_enter)
}

pub fn should_use_shift_insert(process_name: &str, class_name: &str, extra: &str) -> bool {
    let stem = strip_exe(process_name);
    if stem.is_empty() {
        return false;
    }
    if process_listed(extra, &stem) {
        return true;
    }
    if stem.eq_ignore_ascii_case("cmd") {
        return true;
    }
    if !class_name.eq_ignore_ascii_case("ConsoleWindowClass") {
        return false;
    }
    !matches!(
        stem.to_ascii_lowercase().as_str(),
        "windowsterminal" | "wt" | "powershell" | "pwsh"
    )
}

pub fn send_paste(opts: &PasteOptions) -> Result<(), String> {
    send_paste_with_guard(opts, || Ok(()))
}

pub(crate) fn send_paste_with_guard(opts: &PasteOptions, mut verify: impl FnMut() -> Result<(), String>) -> Result<(), String> {
    verify()?;
    if crate::clipboard::is_headless() {
        return Ok(());
    }
    let target = resolve_input_target(opts.expected_target)?;
    target.verify_for_paste()?;
    let (process, _) = foreground_target();
    let delay = resolve_paste_delay_ms(opts.delay_ms, &process);
    dispatch_paste_input(delay.max(0) as u64, &opts.extra_shift_insert, || {
        verify()?;
        target.verify_for_paste()?;
        verify_clipboard_sequence(opts.expected_clipboard_sequence, crate::clipboard::sequence_number())
    }, sleep_input_delay, foreground_target, |shift_insert| {
        if shift_insert {
            send_shift_insert()
        } else {
            send_ctrl_v()
        }
    })
}

fn dispatch_paste_input(
    delay_ms: u64,
    extra_shift_insert: &str,
    verify: impl FnMut() -> Result<(), String>,
    wait: impl FnOnce(u64),
    mut foreground: impl FnMut() -> (String, String),
    send: impl FnOnce(bool) -> Result<(), String>,
) -> Result<(), String> {
    dispatch_checked_input(delay_ms, verify, wait, || {
        // The user can change apps during the configured delay. Choose the
        // shortcut for the application that will actually receive this paste.
        let (process, class_name) = foreground();
        send(should_use_shift_insert(&process, &class_name, extra_shift_insert))
    })
}

fn verify_clipboard_sequence(expected: Option<u32>, current: u32) -> Result<(), String> {
    if expected.is_some_and(|sequence| sequence != current) {
        Err("剪贴板内容已变化，已停止本次粘贴。".into())
    } else {
        Ok(())
    }
}

fn resolve_input_target(target: Option<InputTarget>) -> Result<InputTarget, String> {
    match target {
        Some(target) => Ok(target),
        None => InputTarget::capture(crate::window_menu::foreground_hwnd()),
    }
}

fn sleep_input_delay(delay_ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
}

fn dispatch_checked_input(
    delay_ms: u64,
    mut verify: impl FnMut() -> Result<(), String>,
    wait: impl FnOnce(u64),
    send: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    verify()?;
    if delay_ms > 0 { wait(delay_ms); }
    verify()?;
    send()
}

fn process_listed(list: &str, stem: &str) -> bool {
    crate::clipboard::split_process_names(list)
        .into_iter()
        .map(|name| strip_exe(&name))
        .any(|name| !name.is_empty() && name.eq_ignore_ascii_case(stem))
}

fn strip_exe(name: &str) -> String {
    name.trim()
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE")
        .to_string()
}

pub fn send_ctrl_v() -> Result<(), String> {
    send_chord(false, true, 0x56)
}

pub fn send_ctrl_c() -> Result<(), String> {
    send_chord(false, true, 0x43)
}

/// C# `TextActionRunner.SimulateText` via Unicode `SendInput`.
pub fn simulate_text(text: &str, delay_ms: i32) -> Result<(), String> {
    simulate_text_to(text, delay_ms, None)
}

pub fn simulate_text_to(text: &str, delay_ms: i32, target: Option<InputTarget>) -> Result<(), String> {
    simulate_text_to_with_guard(text, delay_ms, target, || Ok(()))
}

pub(crate) fn simulate_text_to_with_guard(text: &str, delay_ms: i32, target: Option<InputTarget>, mut verify: impl FnMut() -> Result<(), String>) -> Result<(), String> {
    verify()?;
    if crate::clipboard::is_headless() {
        return Ok(());
    }
    let target = resolve_input_target(target)?;
    simulate_text_checked(text, delay_ms, || { verify()?; target.verify_for_paste() }, send_text_character, sleep_input_delay)
}

fn simulate_text_checked(
    text: &str,
    delay_ms: i32,
    mut verify: impl FnMut() -> Result<(), String>,
    mut send: impl FnMut(char) -> Result<(), String>,
    mut wait: impl FnMut(u64),
) -> Result<(), String> {
    let body = text.replace("\r\n", "\n");
    for ch in body.chars() {
        verify()?;
        send(ch)?;
        if delay_ms > 0 {
            wait(delay_ms as u64);
        }
    }
    Ok(())
}

fn send_text_character(ch: char) -> Result<(), String> {
    if crate::clipboard::is_headless() {
        return Ok(());
    }
    // Chromium ignores Unicode CR/LF packets. Simulated typing needs Return
    // for a line break; other characters still use Unicode input below.
    if matches!(ch, '\n' | '\r') {
        return send_enter();
    }
    #[cfg(windows)]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
        };
        let mut units = [0u16; 2];
        let encoded = ch.encode_utf16(&mut units);
        for unit in encoded.iter().copied() {
            let inputs = [unicode_key(unit, false), unicode_key(unit, true)];
            send_keyboard_inputs(&inputs)?;
        }
        return Ok(());

        fn unicode_key(unit: u16, up: bool) -> INPUT {
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: Default::default(),
                        wScan: unit,
                        dwFlags: KEYEVENTF_UNICODE | if up { KEYEVENTF_KEYUP } else { Default::default() },
                        time: 0,
                        dwExtraInfo: HOST_KEYBOARD_INPUT_TAG,
                    },
                },
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = ch;
        Err("simulate_text is Windows-only".into())
    }
}

pub fn send_shift_insert() -> Result<(), String> {
    send_chord(true, false, 0x2D)
}

fn send_chord(shift: bool, ctrl: bool, vk: u16) -> Result<(), String> {
    if crate::clipboard::is_headless() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        send_keyboard_inputs(&chord_inputs(shift, ctrl, vk))
    }
    #[cfg(not(windows))]
    {
        let _ = (shift, ctrl, vk);
        Err("paste is Windows-only".into())
    }
}

#[cfg(windows)]
pub(crate) fn chord_inputs(shift: bool, ctrl: bool, vk: u16) -> Vec<windows::Win32::UI::Input::KeyboardAndMouse::INPUT> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
        VIRTUAL_KEY, VK_CONTROL, VK_INSERT, VK_SHIFT,
    };
    let mut inputs = Vec::new();
    if ctrl {
        inputs.push(key(VK_CONTROL, false));
    }
    if shift {
        inputs.push(key(VK_SHIFT, false));
    }
    inputs.push(key(VIRTUAL_KEY(vk), false));
    inputs.push(key(VIRTUAL_KEY(vk), true));
    if shift {
        inputs.push(key(VK_SHIFT, true));
    }
    if ctrl {
        inputs.push(key(VK_CONTROL, true));
    }
    return inputs;

    fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    // Dedicated Insert is an extended key. Without this
                    // flag Windows applies numeric-keypad Shift handling,
                    // temporarily releasing Shift before delivering Insert.
                    dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() }
                        | if vk == VK_INSERT { KEYEVENTF_EXTENDEDKEY } else { Default::default() },
                    time: 0,
                    dwExtraInfo: HOST_KEYBOARD_INPUT_TAG,
                },
            },
        }
    }
}

/// Keep swallowed Alt/Win triggers from being treated as a bare menu key.
/// Never send a Ctrl-up when the user already holds that modifier.
fn shortcut_menu_needs_disarm(alt_down: bool, win_down: bool, ctrl_down: bool) -> bool {
    (alt_down || win_down) && !ctrl_down
}

pub fn disarm_foreground_shortcut_menu() -> Result<(), String> {
    if crate::clipboard::is_headless() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
            VIRTUAL_KEY, VK_CONTROL, VK_LCONTROL, VK_RCONTROL, VK_MENU, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN,
        };
        let any_down = |keys: &[VIRTUAL_KEY]| keys.iter()
            .any(|key| unsafe { GetAsyncKeyState(key.0 as i32) < 0 });
        // Recheck on the input thread: never release a Ctrl the user is holding,
        // including one pressed after the hook accepted an Alt-only trigger.
        if !shortcut_menu_needs_disarm(
            any_down(&[VK_MENU, VK_LMENU, VK_RMENU]),
            any_down(&[VK_LWIN, VK_RWIN]),
            any_down(&[VK_CONTROL, VK_LCONTROL, VK_RCONTROL]),
        ) {
            return Ok(());
        }
        let inputs = [key(VK_CONTROL, false), key(VK_CONTROL, true)];
        return send_keyboard_inputs(&inputs);

        fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk,
                        wScan: 0,
                        dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() },
                        time: 0,
                        dwExtraInfo: HOST_KEYBOARD_INPUT_TAG,
                    },
                },
            }
        }
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

/// A fast Alt tap may finish before the worker's held-key disarm pulse.
/// Cancel only the confirmed paste target's active menu before clipboard/input.
/// WM_CANCELMODE: https://learn.microsoft.com/windows/win32/winmsg/wm-cancelmode
pub fn cancel_quick_paste_alt_menu_to(target: InputTarget) -> Result<(), String> {
    if crate::clipboard::is_headless() { return Ok(()); }
    let target = target.resolve_for_paste()?;
    target.verify()?;
    #[cfg(windows)]
    unsafe {
        use windows::Win32::{
            Foundation::{LPARAM, WPARAM},
            UI::WindowsAndMessaging::*,
        };
        let foreground = GetForegroundWindow();
        let thread = GetWindowThreadProcessId(foreground, None);
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        GetGUIThreadInfo(thread, &mut info).map_err(|error| error.to_string())?;
        let menu = info.flags.0 & (GUI_INMENUMODE.0 | GUI_POPUPMENUMODE.0 | GUI_SYSTEMMENUMODE.0) != 0;
        let owner = if info.hwndMenuOwner.0.is_null() { info.hwndActive } else { info.hwndMenuOwner };
        let owner_root = GetAncestor(owner, GA_ROOT);
        if !can_cancel_target_menu(menu, foreground.0 as isize, info.hwndActive.0 as isize, owner_root.0 as isize)? {
            return Ok(());
        }
        target.verify()?;
        if SendMessageTimeoutW(owner, WM_CANCELMODE, WPARAM(0), LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_BLOCK, 100, None).0 == 0
        {
            return Err("目标应用未及时退出菜单状态，已停止本次粘贴。".into());
        }
        target.verify()?;
        GetGUIThreadInfo(thread, &mut info).map_err(|error| error.to_string())?;
        if info.flags.0 & (GUI_INMENUMODE.0 | GUI_POPUPMENUMODE.0 | GUI_SYSTEMMENUMODE.0) != 0 {
            return Err("目标应用仍在菜单状态，已停止本次粘贴。".into());
        }
        tracing::debug!("Quick paste canceled the target's Alt menu before input");
    }
    Ok(())
}

fn can_cancel_target_menu(menu: bool, foreground: isize, active: isize, owner_root: isize) -> Result<bool, String> {
    if !menu { return Ok(false); }
    if foreground == 0 || active != foreground || owner_root != foreground {
        return Err("目标窗口或菜单已变化，已停止本次粘贴。".into());
    }
    Ok(true)
}

pub fn send_backspace(count: u8) -> Result<(), String> {
    send_backspace_to(count, None)
}

pub fn send_backspace_to(count: u8, target: Option<crate::input_target::InputTarget>) -> Result<(), String> {
    if crate::clipboard::is_headless() {
        return Ok(());
    }
    for _ in 0..count {
        if let Some(target) = target { target.verify()?; }
        send_chord(false, false, 0x08)?;
    }
    Ok(())
}

pub fn send_enter() -> Result<(), String> {
    if crate::clipboard::is_headless() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_RETURN,
        };
        let inputs = [key(VK_RETURN, false), key(VK_RETURN, true)];
        return send_keyboard_inputs(&inputs);

        fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk,
                        wScan: 0,
                        dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() },
                        time: 0,
                        dwExtraInfo: HOST_KEYBOARD_INPUT_TAG,
                    },
                },
            }
        }
    }
    #[cfg(not(windows))]
    {
        Err("enter is Windows-only".into())
    }
}

#[cfg(windows)]
fn send_keyboard_inputs(inputs: &[windows::Win32::UI::Input::KeyboardAndMouse::INPUT]) -> Result<(), String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT};
    send_keyboard_inputs_with(inputs, |batch| unsafe {
        SendInput(batch, std::mem::size_of::<INPUT>() as i32)
    })
}

/// SendInput returns an event count, not a success boolean. A partial chord must
/// not count as a successful paste or be replayed (the paste may already have fired).
/// https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-sendinput
#[cfg(windows)]
fn send_keyboard_inputs_with(
    inputs: &[windows::Win32::UI::Input::KeyboardAndMouse::INPUT],
    mut send: impl FnMut(&[windows::Win32::UI::Input::KeyboardAndMouse::INPUT]) -> u32,
) -> Result<(), String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_KEYBOARD, KEYEVENTF_KEYUP};
    let sent = send(inputs) as usize;
    if sent == inputs.len() {
        return Ok(());
    }
    // Input events are inserted serially. Release only keys left down in the
    // accepted prefix, in reverse order; never send another key-down on failure.
    let mut pressed: Vec<INPUT> = Vec::new();
    for input in inputs.iter().take(sent) {
        if input.r#type != INPUT_KEYBOARD {
            continue;
        }
        let key = unsafe { input.Anonymous.ki };
        pressed.retain(|previous| {
            let previous = unsafe { previous.Anonymous.ki };
            previous.wVk != key.wVk || previous.wScan != key.wScan
                || (previous.dwFlags & !KEYEVENTF_KEYUP) != (key.dwFlags & !KEYEVENTF_KEYUP)
        });
        if !key.dwFlags.contains(KEYEVENTF_KEYUP) {
            pressed.push(*input);
        }
    }
    let releases: Vec<_> = pressed.into_iter().rev().map(|mut input| {
        unsafe { input.Anonymous.ki.dwFlags |= KEYEVENTF_KEYUP; }
        input
    }).collect();
    let released = if releases.is_empty() { 0 } else { send(&releases) as usize };
    let cleanup = if released == releases.len() {
        String::new()
    } else {
        format!("; key release incomplete ({released}/{})", releases.len())
    };
    Err(format!("SendInput incomplete ({sent}/{} events){cleanup}", inputs.len()))
}

pub fn transform_text(text: &str, newline: bool, standard: bool) -> String {
    let mut body = text.to_string();
    if standard {
        if let Ok(next) = apply_text_action(
            &serde_json::json!({ "Type": "Internal", "Data": { "Type": "StandardText" } }),
            &body,
        ) {
            body = next;
        }
    }
    if newline && !body.ends_with('\n') {
        body.push_str("\r\n");
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn own_input_marker_survives_chord_and_partial_send_cleanup() {
        use std::cell::Cell;
        for (shift, ctrl, vk) in [(false, true, 0x56), (true, false, 0x2D), (false, false, 0x0D)] {
            let inputs = chord_inputs(shift, ctrl, vk);
            let calls = Cell::new(0);
            let error = send_keyboard_inputs_with(&inputs, |batch| {
                calls.set(calls.get() + 1);
                assert!(batch.iter().all(|input| unsafe { input.Anonymous.ki.dwExtraInfo } == HOST_KEYBOARD_INPUT_TAG),
                    "self-generated chord has no ownership marker");
                if calls.get() == 1 { 1 } else { batch.len() as u32 }
            }).unwrap_err();
            assert!(error.contains("SendInput incomplete"));
            assert_eq!(calls.get(), 2, "partial send only releases the injected key");
        }
    }

    #[test]
    fn modifier_release_wait_handles_immediate_and_delayed_release_without_input() {
        use std::cell::Cell;
        for release_at in [0, 30, MODIFIER_RELEASE_TIMEOUT_MS] {
            let elapsed = Cell::new(0);
            let checks = Cell::new(0);
            wait_for_modifier_release_checked(|| {
                checks.set(checks.get() + 1); Ok(())
            }, || elapsed.get() < release_at, || elapsed.get(), |delay| {
                assert!((1..=10).contains(&delay));
                elapsed.set(elapsed.get() + delay);
            }).unwrap();
            assert_eq!(elapsed.get(), release_at);
            assert_eq!(checks.get(), release_at / 10 + 1);
        }
    }

    #[test]
    fn modifier_release_wait_stops_when_request_is_retired_before_reading_keys_again() {
        use std::cell::Cell;
        let checks = Cell::new(0);
        let reads = Cell::new(0);
        let elapsed = Cell::new(0);
        let error = wait_for_modifier_release_checked(|| {
            checks.set(checks.get() + 1);
            if checks.get() == 2 { Err("request retired".into()) } else { Ok(()) }
        }, || { reads.set(reads.get() + 1); true }, || elapsed.get(),
        |delay| elapsed.set(elapsed.get() + delay)).unwrap_err();
        assert_eq!(error, "request retired");
        assert_eq!(reads.get(), 1);
        assert_eq!(elapsed.get(), 10);
    }

    #[test]
    fn modifier_release_wait_is_bounded_and_reports_held_keys() {
        use std::cell::Cell;
        let elapsed = Cell::new(0);
        let error = wait_for_modifier_release_checked(|| Ok(()), || true,
            || elapsed.get(), |delay| elapsed.set(elapsed.get() + delay)).unwrap_err();
        assert_eq!(elapsed.get(), MODIFIER_RELEASE_TIMEOUT_MS);
        assert!(error.contains("修饰键仍按住"));
    }

    #[test]
    fn shortcut_menu_pulse_requires_a_menu_modifier_and_preserves_held_ctrl() {
        assert!(shortcut_menu_needs_disarm(true, false, false));
        assert!(shortcut_menu_needs_disarm(false, true, false));
        assert!(shortcut_menu_needs_disarm(true, true, false));
        assert!(!shortcut_menu_needs_disarm(true, false, true));
        assert!(!shortcut_menu_needs_disarm(false, true, true));
        assert!(!shortcut_menu_needs_disarm(true, true, true));
        assert!(!shortcut_menu_needs_disarm(false, false, false));
        assert!(!shortcut_menu_needs_disarm(false, false, true));
    }

    #[test]
    fn paste_stops_when_clipboard_is_replaced_during_the_delay() {
        use std::cell::Cell;
        for replaced in [false, true] {
            let sequence = Cell::new(u32::MAX);
            let sent = Cell::new(false);
            let result = dispatch_checked_input(100,
                || verify_clipboard_sequence(Some(u32::MAX), sequence.get()),
                |_| { if replaced { sequence.set(0); } }, // wrapping still means changed
                || { sent.set(true); Ok(()) });
            assert_eq!(result.is_ok(), !replaced);
            assert_eq!(sent.get(), !replaced);
        }
    }

    #[test]
    fn paste_shortcut_uses_the_application_foreground_after_the_delay() {
        use std::cell::Cell;
        for (before, after, expected_shift_insert) in [
            ("cmd.exe", "pwsh.exe", false),
            ("pwsh.exe", "cmd.exe", true),
            ("notepad.exe", "custom.exe", true),
        ] {
            let process = Cell::new(before);
            let waited = Cell::new(false);
            dispatch_paste_input(40, "custom", || Ok(()), |delay| {
                assert_eq!(delay, 40);
                waited.set(true);
                process.set(after);
            }, || {
                assert!(waited.get(), "shortcut target was read before the paste delay");
                (process.get().into(), "ConsoleWindowClass".into())
            }, |shift_insert| {
                assert_eq!(shift_insert, expected_shift_insert, "{before} -> {after}");
                Ok(())
            }).unwrap();
        }
    }

    #[test]
    #[cfg(windows)]
    #[ignore = "writes the real Windows clipboard; run alone, never sends keyboard input"]
    fn native_paste_sequence_detects_replacement_without_desktop_input() {
        crate::clipboard::force_headless(false);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "0");
        crate::clipboard::write_text("Rust paste sequence fixture 中文 🦀").unwrap();
        let sequence = crate::clipboard::sequence_number();
        assert_ne!(sequence, 0, "a successful clipboard write needs a readable sequence");
        verify_clipboard_sequence(Some(sequence), crate::clipboard::sequence_number()).unwrap();
        let result = dispatch_checked_input(1,
            || verify_clipboard_sequence(Some(sequence), crate::clipboard::sequence_number()),
            |_| crate::clipboard::write_text("Rust paste sequence replacement").unwrap(),
            || panic!("must not send keys after clipboard replacement"));
        assert!(result.unwrap_err().contains("剪贴板内容已变化"));
    }

    #[test]
    fn delayed_paste_and_enter_stop_when_request_is_retired() {
        use std::cell::Cell;
        for failed_check in [1, 2, 0] {
            let checks = Cell::new(0);
            let waits = Cell::new(0);
            let sends = Cell::new(0);
            let result = dispatch_checked_input(40, || {
                checks.set(checks.get() + 1);
                if checks.get() == failed_check { Err("request retired".into()) } else { Ok(()) }
            }, |delay| {
                assert_eq!(delay, 40);
                waits.set(waits.get() + 1);
            }, || {
                sends.set(sends.get() + 1);
                Ok(())
            });
            assert_eq!(result.is_ok(), failed_check == 0);
            assert_eq!(waits.get(), usize::from(failed_check != 1));
            assert_eq!(sends.get(), usize::from(failed_check == 0));
        }
    }

    #[test]
    fn quick_paste_menu_cancel_preserves_unrelated_windows() {
        assert!(!can_cancel_target_menu(false, 10, 10, 20).unwrap());
        assert!(can_cancel_target_menu(true, 10, 10, 10).unwrap());
        for (foreground, active, owner_root) in [(0, 0, 0), (10, 20, 10), (10, 10, 20)] {
            assert!(can_cancel_target_menu(true, foreground, active, owner_root).is_err());
        }
    }

    #[test]
    fn text_simulation_stops_when_request_is_retired() {
        let mut checked = 0;
        let mut output = String::new();
        let result = simulate_text_checked("甲\r\n😀后续文本", 0, || {
            checked += 1;
            if checked == 3 { Err("request retired".into()) } else { Ok(()) }
        }, |ch| { output.push(ch); Ok(()) }, |_| panic!("zero delay must not wait"));
        assert_eq!(result.unwrap_err(), "request retired");
        assert_eq!(output, "甲\n");
        assert_eq!(checked, 3);
    }

    #[test]
    fn text_simulation_preserves_unicode_with_a_stable_target() {
        let mut output = String::new();
        let mut delays = Vec::new();
        simulate_text_checked("甲\r\n😀\t", 5, || Ok(()), |ch| {
            output.push(ch); Ok(())
        }, |delay| delays.push(delay)).unwrap();
        assert_eq!(output, "甲\n😀\t");
        assert_eq!(delays, vec![5; 4]);
    }

    #[cfg(windows)]
    fn test_input(vk: u16, scan: u16, flags: u32) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk), wScan: scan,
                dwFlags: KEYBD_EVENT_FLAGS(flags), ..Default::default()
            } },
        }
    }

    #[cfg(windows)]
    fn input_keys(inputs: &[windows::Win32::UI::Input::KeyboardAndMouse::INPUT]) -> Vec<(u16, u16, u32)> {
        inputs.iter().map(|input| {
            let key = unsafe { input.Anonymous.ki };
            (key.wVk.0, key.wScan, key.dwFlags.0)
        }).collect()
    }

    #[cfg(windows)]
    #[test]
    fn shift_insert_marks_the_dedicated_key_extended_including_partial_send_cleanup() {
        let chord = chord_inputs(true, false, 0x2d);
        assert_eq!(input_keys(&chord), vec![(0x10, 0, 0), (0x2d, 0, 1), (0x2d, 0, 3), (0x10, 0, 2)]);
        assert_eq!(input_keys(&chord_inputs(false, true, 0x56)), vec![(0x11, 0, 0), (0x56, 0, 0), (0x56, 0, 2), (0x11, 0, 2)]);
        let mut calls = 0;
        let result = send_keyboard_inputs_with(&chord, |batch| {
            calls += 1;
            if calls == 1 { return 2; }
            assert_eq!(input_keys(batch), vec![(0x2d, 0, 3), (0x10, 0, 2)]);
            batch.len() as u32
        });
        assert!(result.is_err());
        assert_eq!(calls, 2);
    }

    #[test]
    #[cfg(windows)]
    fn partial_paste_chord_fails_and_only_releases_injected_keys() {
        use windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_KEYUP;
        let up = KEYEVENTF_KEYUP.0;
        let chord = [test_input(0x11, 0, 0), test_input(0x56, 0, 0), test_input(0x56, 0, up), test_input(0x11, 0, up)];
        for accepted in 0..=chord.len() {
            let mut calls = Vec::new();
            let result = send_keyboard_inputs_with(&chord, |batch| {
                calls.push(input_keys(batch));
                if calls.len() == 1 { accepted as u32 } else { batch.len() as u32 }
            });
            assert_eq!(result.is_ok(), accepted == chord.len());
            match accepted {
                0 | 4 => assert_eq!(calls.len(), 1),
                1 | 3 => assert_eq!(calls[1], vec![(0x11, 0, up)]),
                2 => assert_eq!(calls[1], vec![(0x56, 0, up), (0x11, 0, up)]),
                _ => unreachable!(),
            }
            assert!(calls.iter().skip(1).flatten().all(|(_, _, flags)| flags & up != 0));
        }
    }

    #[test]
    #[cfg(windows)]
    fn partial_unicode_input_releases_the_same_utf16_unit_without_retyping() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{KEYEVENTF_KEYUP, KEYEVENTF_UNICODE};
        let unicode = KEYEVENTF_UNICODE.0;
        let up = KEYEVENTF_KEYUP.0;
        // BMP text and both halves of an emoji use scan codes, with wVk = 0.
        for unit in [0x4e2d, 0xd83d, 0xde00] {
            let inputs = [test_input(0, unit, unicode), test_input(0, unit, unicode | up)];
            let mut calls = Vec::new();
            let result = send_keyboard_inputs_with(&inputs, |batch| {
                calls.push(input_keys(batch));
                1
            });
            assert!(result.unwrap_err().contains("1/2 events"));
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[1], vec![(0, unit, unicode | up)]);
        }
    }

    #[test]
    #[cfg(windows)]
    fn failed_key_release_is_reported_without_retrying_the_chord() {
        let inputs = [test_input(0x11, 0, 0), test_input(0x11, 0, 2)];
        let mut calls = 0;
        let error = send_keyboard_inputs_with(&inputs, |_| {
            calls += 1;
            if calls == 1 { 1 } else { 0 }
        }).unwrap_err();
        assert_eq!(calls, 2);
        assert!(error.contains("key release incomplete (0/1)"));
    }

    #[test]
    fn shift_insert_for_cmd_and_extra() {
        assert!(should_use_shift_insert("cmd.exe", "", ""));
        assert!(should_use_shift_insert("mintty", "", "mintty;foo"));
        assert!(should_use_shift_insert("mintty", "", "foo，mintty"));
        assert!(!should_use_shift_insert("WindowsTerminal.exe", "ConsoleWindowClass", ""));
        assert!(should_use_shift_insert("cmd.exe", "ConsoleWindowClass", ""));
    }

    #[test]
    fn transform_appends_newline() {
        assert_eq!(transform_text("hi", true, false), "hi\r\n");
    }

    #[test]
    fn paste_delay_matches_csharp_quick_paste_and_wechat() {
        assert_eq!(resolve_paste_delay_ms(0, "notepad.exe"), 0);
        assert_eq!(resolve_paste_delay_ms(-1, "notepad.exe"), 50);
        assert_eq!(resolve_paste_delay_ms(200, "notepad.exe"), 200);
        assert_eq!(resolve_paste_delay_ms(0, "WeChat.exe"), 100);
        assert_eq!(resolve_paste_delay_ms(50, "wechat"), 100);
        assert_eq!(resolve_paste_delay_ms(0, "Weixin.exe"), 0);
        assert_eq!(resolve_paste_delay_ms(0, "WeChatAppEx.exe"), 0);
        assert_eq!(QUICK_PASTE_ENTER_DELAY_MS, 30);
        assert_eq!(TOAST_PASTE_ENTER_DELAY_MS, 40);
    }
}
