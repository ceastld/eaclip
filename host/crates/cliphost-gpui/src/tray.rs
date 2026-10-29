//! Tray lifetime and Win32 menu transport for the existing HostState commands.
//! The tray owns its own message thread; it does not reuse the single-instance
//! service's hidden HWND or execute clipboard/input work on the GPUI thread.

use cliphost_core::HostState;

#[cfg(windows)]
pub struct TrayGuard {
    hwnd: isize,
    thread_id: u32,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl TrayGuard {
    pub fn start(host: &HostState) -> Result<Self, String> {
        let host = host.clone();
        let (ready, receiver) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("cliphost-gpui-tray".into())
            .spawn(move || unsafe {
                if let Err(error) = win::run(host, &ready) {
                    let _ = ready.send(Err(error.clone()));
                    eprintln!("ClipHost tray: {error}");
                }
            })
            .map_err(|error| error.to_string())?;
        match receiver.recv().map_err(|error| error.to_string())? {
            Ok((hwnd, thread_id)) => Ok(Self {
                hwnd,
                thread_id,
                worker: Some(worker),
            }),
            Err(error) => {
                let _ = worker.join();
                Err(error)
            }
        }
    }
}

#[cfg(windows)]
impl Drop for TrayGuard {
    fn drop(&mut self) {
        use windows::Win32::{
            Foundation::{HWND, LPARAM, WPARAM},
            UI::WindowsAndMessaging::*,
        };
        unsafe {
            let hwnd = HWND(self.hwnd as *mut _);
            if GetWindowThreadProcessId(hwnd, None) == self.thread_id {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            } else {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(not(windows))]
pub struct TrayGuard;
#[cfg(not(windows))]
impl TrayGuard {
    pub fn start(_: &HostState) -> Result<Self, String> {
        Ok(Self)
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use cliphost_core::i18n::{set_language, tr};
    use std::cell::{Cell, RefCell};
    use std::sync::mpsc::SyncSender;
    use windows::{
        core::{w, PCWSTR},
        Win32::{
            Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
            System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
            UI::{Shell::*, WindowsAndMessaging::*},
        },
    };

    const CALLBACK: u32 = WM_APP + 317;
    const SHOW: usize = 1;
    const HIDE: usize = 2;
    const EXIT: usize = 3;
    const MAIN: usize = 4;
    const FAVORITES: usize = 5;
    const SETTINGS: usize = 6;
    const HELP: usize = 7;
    const SUPER_MENU: usize = 8;
    const LAN_SHARE: usize = 9;
    const LANGUAGE_TIMER: usize = 1;

    fn tooltip() -> &'static str {
        tr("ClipHost GPUI · 剪贴板", "ClipHost GPUI · Clipboard")
    }

    struct TrayState {
        host: HostState,
        icon: HICON,
        taskbar_created: u32,
        tooltip: RefCell<String>,
        visible: Cell<bool>,
    }

    pub unsafe fn run(
        host: HostState,
        ready: &SyncSender<Result<(isize, u32), String>>,
    ) -> Result<(), String> {
        set_language(host.ui_language());
        let instance = HINSTANCE(GetModuleHandleW(None).map_err(|error| error.to_string())?.0);
        let class_name = w!("ClipHostGpui.Tray");
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(format!(
                "Tray class registration failed: {}",
                windows::core::Error::from_win32()
            ));
        }
        // Resource 1 is the same product icon GPUI loads for its window class.
        // LoadIconW returns a shared resource handle; do not destroy it.
        let icon = LoadIconW(Some(instance), PCWSTR(1 as _))
            .map_err(|error| format!("Could not load the ClipHost product icon: {error}"))?;
        let mut state = Box::new(TrayState {
            host,
            icon,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            tooltip: RefCell::new(tooltip().to_owned()),
            visible: Cell::new(false),
        });
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name,
            w!("ClipHost GPUI tray"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            Some((&mut *state as *mut TrayState).cast()),
        )
        .map_err(|error| error.to_string())?;
        if state.host.show_tray_icon() && !notify_icon(hwnd, &state, NIM_ADD) {
            let _ = DestroyWindow(hwnd);
            let _ = UnregisterClassW(class_name, Some(instance));
            return Err("Could not add the ClipHost tray icon.".into());
        }
        state.visible.set(state.host.show_tray_icon());
        // The GPUI maintenance loop applies language changes to the shared
        // process setting. Keep the retained icon in sync without database work.
        SetTimer(Some(hwnd), LANGUAGE_TIMER, 1_000, None);
        let _ = ready.send(Ok((hwnd.0 as isize, GetCurrentThreadId())));
        let mut message = MSG::default();
        let result = loop {
            let status = GetMessageW(&mut message, None, 0, 0).0;
            if status == 0 {
                break Ok(());
            }
            if status < 0 {
                break Err(format!(
                    "Tray message loop failed: {}",
                    windows::core::Error::from_win32()
                ));
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        };
        let _ = notify_icon(hwnd, &state, NIM_DELETE);
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
        let _ = UnregisterClassW(class_name, Some(instance));
        result
    }

    unsafe fn notify_icon(hwnd: HWND, state: &TrayState, message: NOTIFY_ICON_MESSAGE) -> bool {
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: CALLBACK,
            hIcon: state.icon,
            ..Default::default()
        };
        for (destination, ch) in data.szTip.iter_mut().zip(tooltip().encode_utf16()) {
            *destination = ch;
        }
        Shell_NotifyIconW(message, &data).as_bool()
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wp: WPARAM,
        lp: LPARAM,
    ) -> LRESULT {
        if message == WM_NCCREATE {
            let create = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            return LRESULT(1);
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState;
        if let Some(state) = state.as_ref() {
            if state.taskbar_created != 0 && message == state.taskbar_created {
                state.visible.set(state.host.show_tray_icon() && notify_icon(hwnd, state, NIM_ADD));
                return LRESULT(0);
            }
            match message {
                WM_TIMER if wp.0 == LANGUAGE_TIMER => {
                    let visible = state.host.show_tray_icon();
                    if state.visible.get() != visible {
                        let operation = if visible { NIM_ADD } else { NIM_DELETE };
                        if notify_icon(hwnd, state, operation) { state.visible.set(visible); }
                    }
                    let next = tooltip();
                    let changed = state.tooltip.borrow().as_str() != next;
                    if changed && state.visible.get() {
                        *state.tooltip.borrow_mut() = next.to_owned();
                        let _ = notify_icon(hwnd, state, NIM_MODIFY);
                    }
                    return LRESULT(0);
                }
                CALLBACK => {
                    match lp.0 as u32 {
                        WM_LBUTTONUP | WM_LBUTTONDBLCLK => state.host.request_show(),
                        WM_RBUTTONUP | WM_CONTEXTMENU => {
                            if let Err(error) = show_menu(hwnd, state) {
                                eprintln!("ClipHost tray menu: {error}");
                            }
                        }
                        _ => {}
                    }
                    return LRESULT(0);
                }
                WM_CLOSE => {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                WM_DESTROY => {
                    let _ = notify_icon(hwnd, state, NIM_DELETE);
                    PostQuitMessage(0);
                    return LRESULT(0);
                }
                WM_NCDESTROY => {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, message, wp, lp)
    }

    unsafe fn show_menu(hwnd: HWND, state: &TrayState) -> Result<(), String> {
        set_language(state.host.ui_language());
        // The native menu lives on this worker, outside GPUI's themed windows.
        // Read only the in-memory global form and refresh before each opening.
        let form = state.host.settings_form_get(&serde_json::json!({"configId":"global"}));
        let theme = crate::tray_theme::ThemeMode::from_global_form(
            form.as_ref().unwrap_or(&serde_json::Value::Null),
        );
        let _ = crate::tray_theme::apply(hwnd, theme);
        let foreground = GetForegroundWindow();
        let external = if cliphost_core::input_target::belongs_to_host(foreground.0 as isize) {
            cliphost_core::active_window::last_visible_foreground_except(state.host.host_hwnd())
        } else {
            foreground.0 as isize
        };
        let target = cliphost_core::input_target::InputTarget::capture(external).ok();
        let menu = CreatePopupMenu().map_err(|error| error.to_string())?;
        let result = (|| {
            for (id, zh, en) in [
                (MAIN, "打开主窗口", "Open main window"),
                (SHOW, "剪贴板", "Clipboard"),
                (FAVORITES, "收藏", "Favorites"),
                (SUPER_MENU, "超级菜单", "Super menu"),
                (LAN_SHARE, "局域网互传", "LAN sharing"),
                (SETTINGS, "设置", "Settings"),
                (HELP, "帮助", "Help"),
            ] {
                let label: Vec<u16> = tr(zh, en).encode_utf16().chain(Some(0)).collect();
                AppendMenuW(menu, MF_STRING, id, PCWSTR(label.as_ptr()))
                    .map_err(|error| error.to_string())?;
            }
            AppendMenuW(menu, MF_SEPARATOR, 0, None).map_err(|error| error.to_string())?;
            let exit: Vec<u16> = tr("退出", "Exit").encode_utf16().chain(Some(0)).collect();
            AppendMenuW(menu, MF_STRING, EXIT, PCWSTR(exit.as_ptr()))
                .map_err(|error| error.to_string())?;
            let mut point = POINT::default();
            GetCursorPos(&mut point).map_err(|error| error.to_string())?;
            // Win32 requires a foreground menu owner for outside-click dismissal.
            // Its temporary own-process HWND must not become a paste destination.
            let _ = SetForegroundWindow(hwnd);
            let action = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                point.x,
                point.y,
                None,
                hwnd,
                None,
            )
            .0 as usize;
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            if action != EXIT {
                if foreground.0 as isize == external {
                    if let Some(target) = target {
                        target.activate()?;
                    }
                } else if IsWindow(Some(foreground)).as_bool() {
                    let _ = SetForegroundWindow(foreground);
                }
                state.host.notify_accessibility_foreground();
            }
            match action {
                SHOW if target.is_some() => {
                    // The existing verb context preserves the verified external
                    // target even when the clipboard owned focus before the menu.
                    state.host.execute_verb_with_context(
                        "clip show",
                        Some(&cliphost_core::verb::VerbRunContext {
                            target_window_handle: external as i64,
                            ..Default::default()
                        }),
                    )?;
                }
                SHOW => state.host.request_show(),
                FAVORITES => state.host.show_web_route("/favorites"),
                MAIN => state.host.show_web_route("/main/home"),
                SUPER_MENU => state.host.request_super_menu()?,
                LAN_SHARE => state.host.show_web_route("/main/lan"),
                SETTINGS => state.host.show_web_route("/main/settings"),
                HELP => state.host.show_web_route("/main/docs"),
                HIDE => {
                    state.host.window_hide();
                }
                EXIT => {
                    state.host.chrome_web_action(
                        &serde_json::json!({"kind":"mainHostCommand","command":"exit"}),
                    );
                }
                _ => {}
            }
            Ok(())
        })();
        let _ = DestroyMenu(menu);
        result
    }
}
