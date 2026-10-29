//! Presentation and target ownership for the independent native Reopen window.
//! History storage and settings live in reopen_host; these methods never query
//! history or Explorer while holding the host lock.

use super::*;

#[derive(Default)]
pub(super) struct ReopenRuntime {
    pub(super) visible: bool,
    pub(super) hwnd: isize,
    pub(super) menu_undo: Option<Value>,
    generation: u64,
    target_hwnd: i64,
}

#[derive(Debug, Clone)]
pub struct NativeReopenOpenRequest {
    owner: usize,
    hwnd: isize,
    generation: u64,
    target_hwnd: i64,
}

impl HostState {
    pub fn reopen_native_snapshot(&self) -> Value {
        let inner = self.inner.lock();
        let state = &inner.reopen;
        json!({
            "visible": state.visible, "hwnd": state.hwnd,
            "generation": state.generation,
            "targetHwnd": if state.visible { state.target_hwnd } else { inner.last_non_host_fg as i64 },
        })
    }

    pub fn reopen_set_hwnd(&self, hwnd: isize) {
        let mut inner = self.inner.lock();
        if inner.reopen.hwnd != hwnd {
            inner.reopen.hwnd = hwnd;
            inner.reopen.generation = inner.reopen.generation.wrapping_add(1);
            if hwnd == 0 { inner.reopen.visible = false; }
        }
    }

    pub fn reopen_show(&self) {
        self.reopen_show_with_context(None);
    }

    pub fn reopen_show_with_context(&self, context: Option<&crate::verb::VerbRunContext>) {
        let foreground = native_paste_foreground();
        let mut inner = self.inner.lock();
        let requested = context.map_or(0, |context| context.target_window_handle as isize);
        let target = reopen_target(&inner, requested, foreground);
        inner.reopen.target_hwnd = if foreground_is_own_window(&inner, target) { 0 } else { target as i64 };
        inner.reopen.visible = true;
        inner.reopen.generation = inner.reopen.generation.wrapping_add(1);
        inner.chrome.retain(|command| !matches!(command, ChromeCommand::ShowReopen | ChromeCommand::HideReopen));
        push_chrome(&mut inner, ChromeCommand::ShowReopen);
    }

    pub fn reopen_hide(&self) {
        let mut inner = self.inner.lock();
        inner.reopen.visible = false;
        inner.reopen.generation = inner.reopen.generation.wrapping_add(1);
        inner.chrome.retain(|command| !matches!(command, ChromeCommand::ShowReopen | ChromeCommand::HideReopen));
        push_chrome(&mut inner, ChromeCommand::HideReopen);
    }

    pub fn reopen_accept_open(&self) -> Option<NativeReopenOpenRequest> {
        let inner = self.inner.lock();
        let state = &inner.reopen;
        (state.visible && state.hwnd != 0).then(|| NativeReopenOpenRequest {
            owner: Arc::as_ptr(&self.inner) as usize, hwnd: state.hwnd,
            generation: state.generation, target_hwnd: state.target_hwnd,
        })
    }

    pub fn reopen_execute_path(&self, path: &str, request: &NativeReopenOpenRequest, folder_hwnd: i64) -> Result<(), String> {
        let current = {
            let inner = self.inner.lock();
            let state = &inner.reopen;
            request.owner == Arc::as_ptr(&self.inner) as usize
                && state.visible && request.hwnd != 0 && state.hwnd == request.hwnd
                && state.generation == request.generation
        };
        if !current { return Err("重开操作已取消。".into()); }
        if folder_hwnd != 0 {
            self.reopen_opened_path(path, folder_hwnd, request.target_hwnd)
        } else {
            self.reopen_open_path(path, request.target_hwnd)
        }
    }

    pub fn reopen_open_path(&self, path: &str, target_hwnd: i64) -> Result<(), String> {
        if path.trim().is_empty() { return Err("目录路径不能为空。".into()); }
        self.reopen_folder_path(path, target_hwnd)
    }

    pub fn reopen_opened_path(&self, path: &str, folder_hwnd: i64, target_hwnd: i64) -> Result<(), String> {
        if path.trim().is_empty() { return Err("目录路径不能为空。".into()); }
        self.execute_opened_folder_item(path, folder_hwnd, target_hwnd)
    }

    pub fn reopen_copy_path(&self, path: &str) -> Result<(), String> {
        self.copy_text(path)
    }
}

fn reopen_target(inner: &HostInner, requested: isize, foreground: isize) -> isize {
    if requested != 0 && !foreground_is_own_window(inner, requested) { requested }
    else if foreground != 0 && !foreground_is_own_window(inner, foreground) { foreground }
    else { inner.last_non_host_fg }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, HostState) {
        crate::clipboard::force_headless(true);
        let temporary = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&temporary).unwrap();
        let temporary = temporary.canonicalize().unwrap();
        let root = tempfile::Builder::new().prefix("reopen-native-").tempdir_in(temporary).unwrap();
        let paths = AppPaths::from_data_root(root.path().join("data"));
        paths.ensure().unwrap();
        let host = HostState::open_isolated_visual(paths, 29065).unwrap();
        (root, host)
    }

    #[test]
    fn reopened_or_replaced_native_window_rejects_old_open_ticket() {
        let (_root, host) = fixture();
        host.reopen_show();
        host.reopen_set_hwnd(91);
        let old = host.reopen_accept_open().unwrap();
        host.reopen_hide();
        host.reopen_show();
        assert!(host.reopen_execute_path("shell:::fixture", &old, 0).is_err());
        let old = host.reopen_accept_open().unwrap();
        host.reopen_set_hwnd(92);
        assert!(host.reopen_execute_path("shell:::fixture", &old, 0).is_err());
        assert!(host.reopen_accept_open().is_some());
    }

    #[test]
    fn repeated_show_and_hide_keep_only_current_native_command() {
        let (_root, host) = fixture();
        host.reopen_show(); host.reopen_hide(); host.reopen_show();
        let commands: Vec<_> = host.inner.lock().chrome.iter().filter(|command|
            matches!(command, ChromeCommand::ShowReopen | ChromeCommand::HideReopen)).copied().collect();
        assert_eq!(commands, [ChromeCommand::ShowReopen]);
        host.reopen_hide();
        assert_eq!(host.reopen_native_snapshot()["visible"], false);
        assert!(host.reopen_accept_open().is_none());
    }

    #[test]
    fn quick_window_preference_explicit_override_and_automatic_run_are_distinct() {
        let (_root, host) = fixture();
        host.inner.lock().extras.user.re_open_show_menu = true;
        let shown = host.execute_verb("menu reopen").unwrap();
        assert_eq!(shown["native"], true);
        assert_eq!(host.reopen_native_snapshot()["visible"], true);
        host.reopen_hide();
        // This ordinary command would open a real folder. The visual fixture
        // must reject that effect while still proving it did not show a popup.
        let direct = host.execute_verb("menu reopen --show false").unwrap_err();
        assert!(direct.contains("KEYBOARD_QA_SIDE_EFFECT"));
        assert_eq!(host.reopen_native_snapshot()["visible"], false);
        let auto = crate::verb::VerbRunContext { is_automatic_run: true, ..Default::default() };
        let skipped = host.execute_verb_with_context("menu reopen --show true", Some(&auto)).unwrap();
        assert_eq!(skipped["skipped"], "autorun");
        assert_eq!(host.reopen_native_snapshot()["visible"], false);
    }

    #[test]
    fn management_route_does_not_open_a_recent_folder() {
        let (_root, host) = fixture();
        let managed = host.execute_verb("menu reopen --manage true").unwrap();
        assert_eq!(managed["page"], "explorer");
        assert_eq!(host.reopen_native_snapshot()["visible"], false);
        assert!(host.inner.lock().ui.main_visible);
    }

    #[test]
    fn explicit_target_wins_over_the_launcher_foreground() {
        let (_root, host) = fixture();
        let mut inner = host.inner.lock();
        inner.host_hwnd = 11;
        inner.last_non_host_fg = 22;
        assert_eq!(reopen_target(&inner, 33, 44), 33);
        assert_eq!(reopen_target(&inner, 11, 44), 44);
        assert_eq!(reopen_target(&inner, 0, 11), 22);
    }
}
