//! The same product identity as the existing Tauri single-instance plugin.
//! Acquire this before resolving data, opening SQLite, or installing capture.

use std::path::Path;

pub type ActivationReceiver = tokio::sync::mpsc::UnboundedReceiver<()>;

pub const PRODUCT_IDENTIFIER: &str = "net.getquicker.cliphost.tauri";
pub const EXISTING_INSTANCE_PREFIX: &str = "EXISTING_INSTANCE:";

pub fn is_existing_instance(error: &str) -> bool {
    error.starts_with(EXISTING_INSTANCE_PREFIX)
}

pub fn identity(isolated_root: Option<&Path>) -> String {
    match isolated_root {
        None => PRODUCT_IDENTIFIER.into(),
        Some(root) => {
            let digest = blake3::hash(root.to_string_lossy().to_lowercase().as_bytes());
            format!("{PRODUCT_IDENTIFIER}.isolated.{}", &digest.to_hex()[..16])
        }
    }
}

pub struct InstanceGuard {
    identity: String,
    #[cfg(windows)]
    _native: windows_impl::Guard,
    #[cfg(not(windows))]
    _native: portable::Guard,
}

impl InstanceGuard {
    pub fn acquire(
        identity: String,
        compatibility_window: bool,
    ) -> Result<(Self, ActivationReceiver), String> {
        Self::acquire_for_launch(identity, compatibility_window, false)
    }

    pub fn acquire_for_launch(
        identity: String,
        compatibility_window: bool,
        background: bool,
    ) -> Result<(Self, ActivationReceiver), String> {
        #[cfg(windows)]
        let (native, activations) = windows_impl::Guard::acquire(&identity, compatibility_window, background)?;
        #[cfg(not(windows))]
        let (native, activations) = portable::Guard::acquire(&identity, compatibility_window, background)?;
        Ok((
            Self {
                identity,
                #[cfg(windows)]
                _native: native,
                #[cfg(not(windows))]
                _native: native,
            },
            activations,
        ))
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }
}

#[cfg(any(windows, test))]
fn launch_payload_requests_show(payload: &[u8]) -> bool {
    // Tauri's Windows single-instance wire format is UTF-8 cwd|exe|args\0.
    // Inspect the bounded message instead of discarding the forwarded arguments.
    let Some(payload) = payload.strip_suffix(&[0]) else { return false };
    let Ok(payload) = std::str::from_utf8(payload) else { return false };
    let mut parts = payload.split('|');
    if parts.next().is_none() || parts.next().is_none_or(|exe| exe.is_empty()) {
        return false;
    }
    !cliphost_core::startup::is_background_launch(parts)
}

#[cfg(windows)]
mod windows_impl {
    use std::{
        collections::HashMap,
        sync::{mpsc, Mutex, OnceLock},
        thread,
    };
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{
                CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, LRESULT,
                WPARAM,
            },
            System::{
                DataExchange::COPYDATASTRUCT,
                LibraryLoader::GetModuleHandleW,
                Threading::{CreateMutexW, GetCurrentThreadId},
            },
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW,
                GetMessageW, PostThreadMessageW, RegisterClassW, SendMessageTimeoutW,
                TranslateMessage, MSG, SMTO_ABORTIFHUNG, WM_COPYDATA, WM_QUIT, WNDCLASSW,
                WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
            },
        },
    };

    const COPYDATA_ID: usize = 1542;
    static RECEIVERS: OnceLock<Mutex<HashMap<isize, tokio::sync::mpsc::UnboundedSender<()>>>> = OnceLock::new();

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    pub struct Guard {
        mutex: HANDLE,
        window: Option<MessageWindow>,
    }
    // Kernel/window handles are transferred as opaque identity only. Window
    // destruction remains on its creating message thread.
    unsafe impl Send for Guard {}
    unsafe impl Sync for Guard {}

    impl Guard {
        pub fn acquire(
            id: &str,
            compatibility_window: bool,
            background: bool,
        ) -> Result<(Self, super::ActivationReceiver), String> {
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            // Publish the compatible hidden window before creating our mutex.
            // Tauri treats an existing mutex without this window as startup
            // failure and can otherwise fall through into a second DB owner.
            let mut window = if compatibility_window {
                Some(MessageWindow::start(id, sender)?)
            } else {
                None
            };
            let name = wide(&format!("{id}-sim"));
            let mutex = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
                .map_err(|error| format!("Cannot establish product single instance: {error}"))?;
            let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
            if exists {
                // Remove our tentative window before locating the actual owner.
                drop(window.take());
                let _ = unsafe { CloseHandle(mutex) };
                if compatibility_window && !background {
                    notify_existing(id);
                }
                return Err(format!(
                    "{} product host is already running",
                    super::EXISTING_INSTANCE_PREFIX
                ));
            }
            Ok((Self { mutex, window }, receiver))
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            drop(self.window.take());
            let _ = unsafe { CloseHandle(self.mutex) };
        }
    }

    struct MessageWindow {
        thread_id: u32,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl MessageWindow {
        fn start(id: &str, sender: tokio::sync::mpsc::UnboundedSender<()>) -> Result<Self, String> {
            let class = wide(&format!("{id}-sic"));
            let title = wide(&format!("{id}-siw"));
            let (ready, initialized) = mpsc::sync_channel(1);
            let worker = thread::Builder::new()
                .name("cliphost-instance".into())
                .spawn(move || unsafe {
                    let module = match GetModuleHandleW(None) {
                        Ok(module) => module,
                        Err(error) => {
                            let _ = ready.send(Err(error.to_string()));
                            return;
                        }
                    };
                    let registered = WNDCLASSW {
                        lpfnWndProc: Some(window_proc),
                        hInstance: module.into(),
                        lpszClassName: PCWSTR(class.as_ptr()),
                        ..Default::default()
                    };
                    let _ = RegisterClassW(&registered);
                    let hwnd = match CreateWindowExW(
                        WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                        PCWSTR(class.as_ptr()),
                        PCWSTR(title.as_ptr()),
                        WS_POPUP,
                        0,
                        0,
                        0,
                        0,
                        None,
                        None,
                        Some(module.into()),
                        None,
                    ) {
                        Ok(hwnd) => hwnd,
                        Err(error) => {
                            let _ = ready.send(Err(error.to_string()));
                            return;
                        }
                    };
                    RECEIVERS
                        .get_or_init(Default::default)
                        .lock()
                        .unwrap()
                        .insert(hwnd.0 as isize, sender);
                    if ready.send(Ok(GetCurrentThreadId())).is_ok() {
                        let mut message = MSG::default();
                        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                            let _ = TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                    }
                    RECEIVERS
                        .get()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .remove(&(hwnd.0 as isize));
                    let _ = DestroyWindow(hwnd);
                })
                .map_err(|error| error.to_string())?;
            match initialized.recv() {
                Ok(Ok(thread_id)) => Ok(Self {
                    thread_id,
                    worker: Some(worker),
                }),
                other => {
                    let _ = worker.join();
                    Err(format!(
                        "Single-instance notification window failed: {other:?}"
                    ))
                }
            }
        }
    }

    impl Drop for MessageWindow {
        fn drop(&mut self) {
            let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_COPYDATA && lparam.0 != 0 {
            let copy = &*(lparam.0 as *const COPYDATASTRUCT);
            if copy.dwData == COPYDATA_ID {
                if copy.lpData.is_null() || copy.cbData == 0 || copy.cbData > 128 * 1024 {
                    return LRESULT(1);
                }
                let payload = std::slice::from_raw_parts(copy.lpData as *const u8, copy.cbData as usize);
                if !super::launch_payload_requests_show(payload) {
                    return LRESULT(1);
                }
                if let Some(receiver) = RECEIVERS.get().and_then(|map| {
                    map.lock()
                        .ok()
                        .and_then(|map| map.get(&(hwnd.0 as isize)).cloned())
                }) {
                    let _ = receiver.send(());
                }
                return LRESULT(1);
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }

    fn notify_existing(id: &str) {
        let class = wide(&format!("{id}-sic"));
        let title = wide(&format!("{id}-siw"));
        unsafe {
            let Ok(hwnd) = FindWindowW(PCWSTR(class.as_ptr()), PCWSTR(title.as_ptr())) else {
                return;
            };
            let cwd = std::env::current_dir().unwrap_or_default();
            let body = format!(
                "{}|{}\0",
                cwd.display(),
                std::env::args().collect::<Vec<_>>().join("|")
            );
            let data = COPYDATASTRUCT {
                dwData: COPYDATA_ID,
                cbData: body.len() as u32,
                lpData: body.as_ptr() as *mut _,
            };
            let _ = SendMessageTimeoutW(
                hwnd,
                WM_COPYDATA,
                WPARAM(0),
                LPARAM(&data as *const _ as isize),
                SMTO_ABORTIFHUNG,
                2_000,
                None,
            );
        }
    }
}

#[cfg(not(windows))]
mod portable {
    use std::{
        collections::HashSet,
        sync::{Mutex, OnceLock},
    };
    static IDENTITIES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    pub struct Guard(String);
    impl Guard {
        pub fn acquire(id: &str, _: bool, _: bool) -> Result<(Self, super::ActivationReceiver), String> {
            if !IDENTITIES
                .get_or_init(Default::default)
                .lock()
                .unwrap()
                .insert(id.into())
            {
                return Err(format!(
                    "{} product host is already running",
                    super::EXISTING_INSTANCE_PREFIX
                ));
            }
            let (_, receiver) = tokio::sync::mpsc::unbounded_channel();
            Ok((Self(id.into()), receiver))
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            IDENTITIES.get().unwrap().lock().unwrap().remove(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatible_launch_payload_keeps_background_and_explicit_launch_separate() {
        assert!(launch_payload_requests_show(b"C:\\App|C:\\App\\cliphost-app.exe\0"));
        assert!(launch_payload_requests_show(b"C:\\App|cliphost-app.exe|--cliphost-demote-attempted\0"));
        assert!(!launch_payload_requests_show(b"C:\\App|cliphost-app.exe|--background\0"));
        assert!(!launch_payload_requests_show(b"C:\\App|cliphost-app.exe|--background|--cliphost-restart-parent=42:123\0"));
        for malformed in [b"no-argv\0".as_slice(), b"cwd|\0", b"cwd|exe", b"cwd|exe|\xff\0"] {
            assert!(!launch_payload_requests_show(malformed));
        }
    }
    #[test]
    fn same_identity_is_rejected_until_guard_is_dropped() {
        let name = format!("{PRODUCT_IDENTIFIER}.unit.{}", std::process::id());
        let (first, _) = InstanceGuard::acquire(name.clone(), false).unwrap();
        assert!(is_existing_instance(
            &InstanceGuard::acquire(name.clone(), false).err().unwrap()
        ));
        drop(first);
        assert!(InstanceGuard::acquire(name, false).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn secondary_launch_notifies_the_compatible_primary_window() {
        let name = format!("{PRODUCT_IDENTIFIER}.ipc-unit.{}", std::process::id());
        let (_primary, mut activation) = InstanceGuard::acquire(name.clone(), true).unwrap();
        let error = InstanceGuard::acquire(name, true).err().unwrap();
        assert!(is_existing_instance(&error));
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(3), activation.recv()).await.unwrap().unwrap();
        });
    }

    #[cfg(windows)]
    #[test]
    fn background_secondary_launch_leaves_the_compatible_primary_hidden() {
        let name = format!("{PRODUCT_IDENTIFIER}.background-ipc-unit.{}", std::process::id());
        let (_primary, mut activation) = InstanceGuard::acquire(name.clone(), true).unwrap();
        let error = InstanceGuard::acquire_for_launch(name.clone(), true, true).err().unwrap();
        assert!(is_existing_instance(&error));
        assert!(activation.try_recv().is_err());
        let error = InstanceGuard::acquire_for_launch(name, true, false).err().unwrap();
        assert!(is_existing_instance(&error));
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(3), activation.recv()).await.unwrap().unwrap();
        });
    }
}
