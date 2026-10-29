//! C# `ToolWindow.InputText` / `InputTextWindow` (title 输入文本, 确认(_S) / 取消(_C)).

/// Window title set by C# `ToolWindow.InputText` after `InputTextWindow` construction.
pub const INPUT_TEXT_WINDOW_TITLE: &str = "输入文本";

/// `None` = cancel (`ShowDialog` false). `Some` even when the text is empty (C# still returns `win.Text`).
pub fn prompt_input_text(initial: &str, owner_hwnd: i64) -> Option<String> {
    if crate::clipboard::is_headless() || !crate::clipboard::native_dialogs_enabled() {
        return None;
    }
    #[cfg(windows)]
    {
        prompt_input_text_windows(initial, owner_hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = (initial, owner_hwnd);
        None
    }
}

#[cfg(windows)]
fn prompt_input_text_windows(initial: &str, owner_hwnd: i64) -> Option<String> {
    use std::cell::RefCell;
    use std::sync::Once;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{GetStockObject, GetSysColorBrush, COLOR_WINDOW, DEFAULT_GUI_FONT};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        GetMessageW, GetWindowTextLengthW, GetWindowTextW, IsWindow,
        PostQuitMessage, RegisterClassW, SetWindowTextW, ShowWindow,
        TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, HMENU, MSG,
        SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY,
        WM_SETFONT, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_POPUP,
        WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
    };

    const IDOK: usize = 1;
    const IDCANCEL: usize = 2;
    const ID_EDIT: usize = 100;

    struct PromptState {
        edit: HWND,
        accepted: bool,
        text: String,
    }

    thread_local! {
        static STATE: RefCell<Option<PromptState>> = const { RefCell::new(None) };
    }

    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: w!("ClipHostInputText"),
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            ..Default::default()
        };
        let _ = RegisterClassW(&class);
    });

    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_CREATE => {
                let hinstance = GetModuleHandleW(None).unwrap_or_default();
                let font = GetStockObject(DEFAULT_GUI_FONT);
                let edit = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("EDIT"),
                    w!(""),
                    WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | WS_TABSTOP.0),
                    10,
                    12,
                    360,
                    24,
                    hwnd,
                    HMENU(ID_EDIT as *mut _),
                    hinstance,
                    None,
                )
                .unwrap_or_default();
                let ok = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("BUTTON"),
                    w!("确认(_S)"),
                    WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0),
                    210,
                    48,
                    80,
                    26,
                    hwnd,
                    HMENU(IDOK as *mut _),
                    hinstance,
                    None,
                )
                .unwrap_or_default();
                let cancel = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("BUTTON"),
                    w!("取消(_C)"),
                    WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0),
                    298,
                    48,
                    80,
                    26,
                    hwnd,
                    HMENU(IDCANCEL as *mut _),
                    hinstance,
                    None,
                )
                .unwrap_or_default();
                let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                    edit,
                    WM_SETFONT,
                    WPARAM(font.0 as usize),
                    LPARAM(1),
                );
                let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                    ok,
                    WM_SETFONT,
                    WPARAM(font.0 as usize),
                    LPARAM(1),
                );
                let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                    cancel,
                    WM_SETFONT,
                    WPARAM(font.0 as usize),
                    LPARAM(1),
                );
                STATE.with(|slot| {
                    if let Some(state) = slot.borrow_mut().as_mut() {
                        state.edit = edit;
                        let wide: Vec<u16> = state
                            .text
                            .encode_utf16()
                            .chain(std::iter::once(0))
                            .collect();
                        let _ = SetWindowTextW(edit, PCWSTR(wide.as_ptr()));
                    }
                });
                let _ = SetFocus(edit);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = wparam.0 & 0xffff;
                if id == IDOK || id == IDCANCEL {
                    STATE.with(|slot| {
                        if let Some(state) = slot.borrow_mut().as_mut() {
                            if id == IDOK {
                                state.accepted = true;
                                state.text = read_window_text(state.edit);
                            } else {
                                state.accepted = false;
                            }
                        }
                    });
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                STATE.with(|slot| {
                    if let Some(state) = slot.borrow_mut().as_mut() {
                        state.accepted = false;
                    }
                });
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    fn read_window_text(hwnd: HWND) -> String {
        unsafe {
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return String::new();
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(hwnd, &mut buf);
            String::from_utf16_lossy(&buf[..n as usize])
        }
    }

    let owner = HWND(owner_hwnd as *mut _);
    let owner_ok = owner_hwnd != 0 && unsafe { IsWindow(owner).as_bool() };
    STATE.with(|slot| {
        *slot.borrow_mut() = Some(PromptState {
            edit: HWND::default(),
            accepted: false,
            text: initial.to_string(),
        });
    });

    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let title: Vec<u16> = INPUT_TEXT_WINDOW_TITLE
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("ClipHostInputText"),
            PCWSTR(title.as_ptr()),
            WINDOW_STYLE(WS_POPUP.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_VISIBLE.0),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            400,
            120,
            if owner_ok { owner } else { HWND::default() },
            HMENU::default(),
            hinstance,
            None,
        )
        .ok()?;
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    STATE.with(|slot| {
        slot.borrow_mut().take().and_then(|state| {
            if state.accepted {
                Some(state.text)
            } else {
                None
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_input_text_headless_cancels_like_file_dialogs() {
        crate::clipboard::force_headless(true);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        assert_eq!(prompt_input_text("seed", 0), None);
        assert_eq!(INPUT_TEXT_WINDOW_TITLE, "输入文本");
    }
}
