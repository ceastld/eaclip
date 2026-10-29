//! Ownership for independent and menu-embedded file browsing. Filesystem work
//! is never performed while the host lock is held.

use super::*;

#[derive(Default)]
pub(super) struct FileBrowserRuntime {
    pub(super) visible: bool,
    pub(super) hwnd: isize,
    generation: u64,
    path: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileBrowserNativeSnapshot {
    pub visible: bool,
    pub hwnd: isize,
    pub generation: u64,
    /// Empty means the browser's own home page.
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct NativeFileBrowserOpenRequest {
    owner: usize,
    surface: FileBrowserOpenSurface,
    claimed: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
enum FileBrowserOpenSurface {
    Window { hwnd: isize, generation: u64 },
    Panel(NativeFileBrowserPanelLease),
}

/// A panel lease owns one appearance inside one accepted menu presentation.
/// Clones share permanent retirement, while a new panel gets an independent
/// lease. Retiring one panel cannot affect a replacement or standalone window.
#[derive(Debug, Clone)]
pub struct NativeFileBrowserPanelLease {
    owner: usize,
    parent: NativeSuperMenuRequest,
    alive: Arc<AtomicBool>,
}

impl NativeFileBrowserPanelLease {
    pub fn retire(&self) {
        self.alive.store(false, Ordering::Release);
    }

    /// Memory-only panel state. Use the host's lease check when parent-menu
    /// lifecycle and host identity also need to be verified.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

impl HostState {
    pub fn file_browser_create_panel_lease(
        &self,
        parent: &NativeSuperMenuRequest,
    ) -> Result<NativeFileBrowserPanelLease, String> {
        if !self.native_super_menu_request_is_current(parent) {
            return Err("文件浏览面板已关闭。".into());
        }
        Ok(NativeFileBrowserPanelLease {
            owner: Arc::as_ptr(&self.inner) as usize,
            parent: parent.clone(),
            alive: Arc::new(AtomicBool::new(true)),
        })
    }

    pub fn file_browser_panel_lease_is_current(&self, lease: &NativeFileBrowserPanelLease) -> bool {
        lease.owner == Arc::as_ptr(&self.inner) as usize
            && lease.is_alive()
            && self.native_super_menu_request_is_current(&lease.parent)
            // Parent validation may wait briefly on its registry. A concurrent
            // panel close during that wait still retires this observation.
            && lease.is_alive()
    }

    pub fn file_browser_capture_panel_open(
        &self,
        lease: &NativeFileBrowserPanelLease,
    ) -> Result<NativeFileBrowserOpenRequest, String> {
        if !self.file_browser_panel_lease_is_current(lease) {
            return Err("文件打开操作已取消。".into());
        }
        Ok(NativeFileBrowserOpenRequest {
            owner: Arc::as_ptr(&self.inner) as usize,
            surface: FileBrowserOpenSurface::Panel(lease.clone()),
            claimed: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn file_browser_snapshot(&self) -> FileBrowserNativeSnapshot {
        let inner = self.inner.lock();
        let browser = &inner.file_browser;
        FileBrowserNativeSnapshot {
            visible: browser.visible,
            hwnd: browser.hwnd,
            generation: browser.generation,
            path: browser.path.clone(),
        }
    }

    pub fn file_browser_set_hwnd(&self, hwnd: isize) {
        let mut inner = self.inner.lock();
        let browser = &mut inner.file_browser;
        if browser.hwnd != hwnd {
            browser.hwnd = hwnd;
            browser.generation = browser.generation.wrapping_add(1);
            if hwnd == 0 {
                browser.visible = false;
            }
        }
    }

    pub fn file_browser_show(&self, path: &str) -> Result<(), String> {
        // Lexical validation only: opening the window never waits for a drive.
        let path = if path.trim().is_empty() {
            String::new()
        } else {
            crate::file_browser::browser_normalize_path(path)?
        };
        let mut inner = self.inner.lock();
        inner.file_browser.visible = true;
        inner.file_browser.path = path;
        inner.file_browser.generation = inner.file_browser.generation.wrapping_add(1);
        drop_browser_commands(&mut inner);
        push_chrome(&mut inner, ChromeCommand::ShowFileBrowser);
        Ok(())
    }

    pub fn file_browser_hide(&self) {
        let mut inner = self.inner.lock();
        inner.file_browser.visible = false;
        inner.file_browser.generation = inner.file_browser.generation.wrapping_add(1);
        drop_browser_commands(&mut inner);
        push_chrome(&mut inner, ChromeCommand::HideFileBrowser);
    }

    pub fn file_browser_capture_open(&self) -> Result<NativeFileBrowserOpenRequest, String> {
        let inner = self.inner.lock();
        let browser = &inner.file_browser;
        if !browser.visible || browser.hwnd == 0 {
            return Err("文件打开操作已取消。".into());
        }
        Ok(NativeFileBrowserOpenRequest {
            owner: Arc::as_ptr(&self.inner) as usize,
            surface: FileBrowserOpenSurface::Window {
                hwnd: browser.hwnd,
                generation: browser.generation,
            },
            claimed: Arc::new(AtomicBool::new(false)),
        })
    }

    fn file_browser_verify_open(
        &self,
        request: &NativeFileBrowserOpenRequest,
    ) -> Result<(), String> {
        if request.owner != Arc::as_ptr(&self.inner) as usize {
            return Err("文件打开操作已取消。".into());
        }
        let current = match &request.surface {
            FileBrowserOpenSurface::Window { hwnd, generation } => {
                let inner = self.inner.lock();
                let browser = &inner.file_browser;
                browser.visible
                    && *hwnd != 0
                    && browser.hwnd == *hwnd
                    && browser.generation == *generation
            }
            FileBrowserOpenSurface::Panel(lease) => self.file_browser_panel_lease_is_current(lease),
        };
        if !current {
            return Err("文件打开操作已取消。".into());
        }
        Ok(())
    }

    /// Call on a worker. Directories are navigated by the view, never by Shell.
    pub fn file_browser_open_file(
        &self,
        path: &str,
        request: &NativeFileBrowserOpenRequest,
    ) -> Result<(), String> {
        self.file_browser_open_file_after_metadata(path, request, || {})
    }

    fn file_browser_open_file_after_metadata(
        &self,
        path: &str,
        request: &NativeFileBrowserOpenRequest,
        after_metadata: impl FnOnce(),
    ) -> Result<(), String> {
        self.file_browser_verify_open(request)?;
        if request.claimed.swap(true, Ordering::AcqRel) {
            return Err("文件打开操作已处理。".into());
        }
        self.require_external_effects("open a file from the file browser")?;
        let path = crate::file_browser::browser_normalize_path(path)?;
        let metadata = std::fs::metadata(&path);
        after_metadata();
        self.file_browser_verify_open(request)?;
        let metadata = metadata.map_err(|error| format!("无法打开文件：{error}"))?;
        if metadata.is_dir() {
            return Err("请在浏览栏中打开此目录。".into());
        }
        self.file_browser_verify_open(request)?;
        self.open_shell_target_guarded(&path, &|| self.file_browser_verify_open(request))
    }
}

fn drop_browser_commands(inner: &mut HostInner) {
    inner.chrome.retain(|command| {
        !matches!(
            command,
            ChromeCommand::ShowFileBrowser | ChromeCommand::HideFileBrowser
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, HostState) {
        crate::clipboard::force_headless(true);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&root).unwrap();
        let root = tempfile::Builder::new()
            .prefix("file-browser-native-")
            .tempdir_in(root.canonicalize().unwrap())
            .unwrap();
        let paths = AppPaths::from_data_root(root.path().join("data"));
        paths.ensure().unwrap();
        let host = HostState::open_isolated_visual(paths, 29065).unwrap();
        (root, host)
    }

    fn panel_parent(host: &HostState) -> (NativeSuperMenuRequest, String) {
        // Register a synthetic menu through the real registry; never enumerate
        // production Explorer windows or file-menu roots to create a ticket.
        let context = crate::menu_api::MenuBuildContext {
            source_window_handle: 200,
            ..Default::default()
        };
        let node =
            crate::menu_api::command("owned panel", "setting", json!({ "page": "menu_file" }));
        let created = host.menus.create_fixture_session(&context, vec![node]);
        let session = created["sessionId"].as_str().unwrap().to_string();
        host.set_super_menu_hwnd(200);
        let generation = {
            let mut inner = host.inner.lock();
            inner.window_runtime_ready = true;
            inner.super_menu.generation = inner.super_menu.generation.wrapping_add(1);
            inner.super_menu.visible = true;
            inner.super_menu.session_id = Some(session.clone());
            inner.super_menu.snapshot = Some(created);
            inner.super_menu.generation
        };
        (
            host.capture_native_super_menu_action(&session, generation, 200)
                .unwrap(),
            session,
        )
    }

    fn normalized_fixture_path(path: &std::path::Path) -> String {
        let path = path.to_string_lossy();
        crate::file_browser::browser_normalize_path(path.strip_prefix(r"\\?\").unwrap_or(&path))
            .unwrap()
    }

    #[test]
    fn file_browser_panel_lease_cannot_cross_host_or_revive_after_panel_close() {
        let (_root, host) = fixture();
        let (_other_root, other) = fixture();
        let (parent, _) = panel_parent(&host);
        let lease = host.file_browser_create_panel_lease(&parent).unwrap();
        let clone = lease.clone();
        let request = host.file_browser_capture_panel_open(&lease).unwrap();
        assert!(host.file_browser_panel_lease_is_current(&lease));
        assert!(
            !host.file_browser_snapshot().visible,
            "an embedded lease must not show the independent window"
        );
        assert!(!other.file_browser_panel_lease_is_current(&lease));
        assert!(other.file_browser_create_panel_lease(&parent).is_err());
        assert!(other.file_browser_verify_open(&request).is_err());
        clone.retire();
        assert!(!lease.is_alive());
        assert!(host.file_browser_verify_open(&request).is_err());
        assert!(host.file_browser_capture_panel_open(&lease).is_err());
        let replacement = host.file_browser_create_panel_lease(&parent).unwrap();
        assert!(host.file_browser_panel_lease_is_current(&replacement));
        assert!(!host.file_browser_panel_lease_is_current(&lease));
    }

    #[test]
    fn file_browser_panel_parent_hide_replace_and_hwnd_change_cancel_open_requests() {
        let (_root, host) = fixture();
        for retirement in ["hide", "replace", "hwnd"] {
            let (parent, session) = panel_parent(&host);
            let lease = host.file_browser_create_panel_lease(&parent).unwrap();
            let request = host.file_browser_capture_panel_open(&lease).unwrap();
            match retirement {
                "hide" => {
                    assert!(host.menu_close_session(&session));
                }
                "replace" => {
                    let _ = panel_parent(&host);
                }
                _ => host.set_super_menu_hwnd(201),
            }
            assert!(
                !host.file_browser_panel_lease_is_current(&lease),
                "{retirement}"
            );
            assert!(
                host.file_browser_verify_open(&request).is_err(),
                "{retirement}"
            );
            assert!(
                host.file_browser_capture_panel_open(&lease).is_err(),
                "{retirement}"
            );
        }
    }

    #[test]
    fn file_browser_panel_open_is_one_use_and_rechecks_retirement_after_metadata() {
        let (root, _visual) = fixture();
        let paths = AppPaths::from_data_root(root.path().join("panel-open-data"));
        paths.ensure().unwrap();
        let host = HostState::open(paths, 29065).unwrap();
        let opened = Arc::new(Mutex::new(Vec::new()));
        let captured = opened.clone();
        host.set_shell_open_runner(move |path| {
            captured.lock().push(path.to_string());
            Ok(())
        });
        let file = root.path().join("panel-file.txt");
        std::fs::write(&file, b"owned embedded browser fixture").unwrap();
        let path = normalized_fixture_path(&file);
        let (parent, _) = panel_parent(&host);
        let lease = host.file_browser_create_panel_lease(&parent).unwrap();
        let request = host.file_browser_capture_panel_open(&lease).unwrap();
        host.file_browser_open_file(&path, &request).unwrap();
        assert!(host
            .file_browser_open_file(&path, &request.clone())
            .unwrap_err()
            .contains("已处理"));
        assert_eq!(&*opened.lock(), &[path.clone()]);
        for retirement in ["panel", "parent"] {
            let (parent, session) = panel_parent(&host);
            let lease = host.file_browser_create_panel_lease(&parent).unwrap();
            let request = host.file_browser_capture_panel_open(&lease).unwrap();
            assert!(host
                .file_browser_open_file_after_metadata(&path, &request, || {
                    if retirement == "panel" {
                        lease.retire();
                    } else {
                        host.menu_close_session(&session);
                    }
                })
                .unwrap_err()
                .contains("已取消"));
            assert_eq!(
                opened.lock().len(),
                1,
                "{retirement} must cancel Shell dispatch"
            );
        }
        assert!(!host.file_browser_snapshot().visible);
    }

    #[test]
    fn file_browser_hide_reshow_and_hwnd_replacement_retire_open_requests() {
        let (_root, host) = fixture();
        host.file_browser_show("").unwrap();
        host.file_browser_set_hwnd(71);
        let request = host.file_browser_capture_open().unwrap();
        host.file_browser_hide();
        assert!(host.file_browser_verify_open(&request).is_err());
        host.file_browser_show("C:\\").unwrap();
        assert!(host.file_browser_verify_open(&request).is_err());
        let request = host.file_browser_capture_open().unwrap();
        host.file_browser_set_hwnd(72);
        assert!(host.file_browser_verify_open(&request).is_err());
        host.file_browser_set_hwnd(0);
        assert!(!host.file_browser_snapshot().visible);
    }

    #[test]
    fn file_browser_show_is_lexical_and_keeps_only_the_latest_intent() {
        let (_root, host) = fixture();
        host.file_browser_show("Z:\\not-probed").unwrap();
        host.file_browser_hide();
        host.file_browser_show("").unwrap();
        let commands: Vec<_> = host
            .inner
            .lock()
            .chrome
            .iter()
            .filter(|command| {
                matches!(
                    command,
                    ChromeCommand::ShowFileBrowser | ChromeCommand::HideFileBrowser
                )
            })
            .copied()
            .collect();
        assert_eq!(commands, [ChromeCommand::ShowFileBrowser]);
        assert!(host.file_browser_snapshot().path.is_empty());
        assert!(host.file_browser_show("shell:::{fixture}").is_err());
    }

    #[test]
    fn file_browser_open_ticket_cannot_cross_host_or_replay() {
        let (_root, host) = fixture();
        let (_other_root, other) = fixture();
        host.file_browser_show("").unwrap();
        host.file_browser_set_hwnd(71);
        let request = host.file_browser_capture_open().unwrap();
        assert!(other.file_browser_verify_open(&request).is_err());
        assert!(host
            .file_browser_open_file("C:\\fixture.txt", &request)
            .unwrap_err()
            .contains("KEYBOARD_QA_SIDE_EFFECT"));
        assert!(host
            .file_browser_open_file("C:\\fixture.txt", &request)
            .unwrap_err()
            .contains("已处理"));
    }

    #[test]
    fn file_browser_opens_only_the_selected_file_and_keeps_directories_internal() {
        let (root, _visual) = fixture();
        let paths = AppPaths::from_data_root(root.path().join("file-open-data"));
        paths.ensure().unwrap();
        let host = HostState::open(paths, 29065).unwrap();
        let opened = Arc::new(Mutex::new(Vec::new()));
        let captured = opened.clone();
        host.set_shell_open_runner(move |path| {
            captured.lock().push(path.to_string());
            Ok(())
        });
        host.file_browser_show("").unwrap();
        host.file_browser_set_hwnd(71);
        let file = root.path().join("sample.txt");
        std::fs::write(&file, b"owned file browser fixture").unwrap();
        let file_text = file.to_string_lossy();
        let path = crate::file_browser::browser_normalize_path(
            file_text.strip_prefix(r"\\?\").unwrap_or(&file_text),
        )
        .unwrap();
        let ticket = host.file_browser_capture_open().unwrap();
        host.file_browser_open_file(&path, &ticket).unwrap();
        assert_eq!(&*opened.lock(), &[path]);
        let ticket = host.file_browser_capture_open().unwrap();
        let directory = root.path().to_string_lossy();
        let directory = directory.strip_prefix(r"\\?\").unwrap_or(&directory);
        assert!(host
            .file_browser_open_file(directory, &ticket)
            .unwrap_err()
            .contains("请在浏览栏中打开"));
        assert_eq!(opened.lock().len(), 1);
    }

    #[test]
    fn file_browser_foreground_keeps_text_navigation_and_selection_keys() {
        let (_root, host) = fixture();
        host.file_browser_show("").unwrap();
        host.file_browser_set_hwnd(71);
        let mut inner = host.inner.lock();
        inner.window_runtime_ready = true;
        inner.window_visible = true;
        inner.clipboard_web_view_ready = true;
        inner.has_host_fake_focus = true;
        inner.no_focus_session_active = true;
        inner.config.enable_vv_tab_text_instruction = true;
        inner.chrome.clear();
        for (vk, ctrl, alt) in [
            (0x31, false, false),
            (0x41, true, false),
            (0x43, true, false),
            (0x4C, true, false),
            (0x0D, false, false),
            (0x08, false, false),
            (0x1B, false, false),
            (0x25, false, true),
            (0x26, false, true),
            (0x56, false, false),
            (0x56, false, false),
            (0x09, false, false),
        ] {
            let key = KeyEvent {
                vk,
                ctrl,
                alt,
                shift: false,
                win: false,
                down: true,
                now_ms: 1,
            };
            assert!(
                !host.input_dispatch_low_level_key_for_foreground(&mut inner, key, 71, true),
                "vk={vk}"
            );
            assert!(!host.input_dispatch_low_level_key_for_foreground(
                &mut inner,
                KeyEvent { down: false, ..key },
                71,
                false
            ));
        }
        assert!(inner.vv_tab_pending.is_none());
        assert!(inner.chrome.is_empty());
        assert!(foreground_is_own_window(&inner, 71));
    }
}
