//! A settings form can live in the clipboard, panel or main window. Capture
//! its registered display when the click is accepted, before queuing a picker.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickerSurface {
    Clipboard,
    Panel,
    Main,
}

impl PickerSurface {
    fn index(self) -> usize {
        match self {
            Self::Clipboard => 0,
            Self::Panel => 1,
            Self::Main => 2,
        }
    }

    fn state(self, inner: &HostInner) -> (isize, bool, bool) {
        match self {
            Self::Clipboard => (
                inner.host_hwnd,
                inner.window_visible,
                inner.clipboard_web_view_ready,
            ),
            Self::Panel => (
                inner.panel_hwnd,
                inner.ui.panel_visible,
                inner.panel_web_view_ready,
            ),
            Self::Main => (inner.main_hwnd, inner.ui.main_visible, true),
        }
    }
}

/// An accepted native picker keeps the original Host and registered root.
/// Keyboard focus/list epochs may change while its modal is active; only the
/// captured window and display lifetimes own this selection.
#[derive(Clone)]
pub struct NativePickerWindowRequest {
    host: HostState,
    source_hwnd: isize,
    surface: PickerSurface,
    window_generation: u64,
    presentation_generation: u64,
}

fn retired_picker() -> String {
    crate::file_dialog_pick::NATIVE_PICKER_RETIRED.to_string()
}

impl HostState {
    /// Called on the UI thread at acceptance, never recaptured by a worker.
    pub fn capture_native_picker_window(
        &self,
        source_hwnd: isize,
    ) -> Result<NativePickerWindowRequest, String> {
        let inner = self.inner.lock();
        if source_hwnd == 0 || !inner.window_runtime_ready {
            return Err(retired_picker());
        }
        let mut surfaces = [PickerSurface::Clipboard, PickerSurface::Panel, PickerSurface::Main]
            .into_iter()
            .filter(|surface| surface.state(&inner).0 == source_hwnd);
        let surface = surfaces.next().ok_or_else(retired_picker)?;
        // Ambiguous registration must not let a caller borrow another root.
        if surfaces.next().is_some() {
            return Err(retired_picker());
        }
        let (_, visible, ready) = surface.state(&inner);
        if !visible || !ready {
            return Err(retired_picker());
        }
        let index = surface.index();
        Ok(NativePickerWindowRequest {
            host: self.clone(),
            source_hwnd,
            surface,
            window_generation: inner.chrome_input_epochs[index].window,
            presentation_generation: inner.chrome_presentation_epochs[index],
        })
    }

    pub fn native_picker_window_is_current(&self, request: &NativePickerWindowRequest) -> bool {
        if !Arc::ptr_eq(&self.inner, &request.host.inner) {
            return false;
        }
        let inner = self.inner.lock();
        let (hwnd, visible, ready) = request.surface.state(&inner);
        let index = request.surface.index();
        inner.window_runtime_ready
            && ready
            && visible
            && hwnd != 0
            && hwnd == request.source_hwnd
            && inner.chrome_input_epochs[index].window == request.window_generation
            && inner.chrome_presentation_epochs[index] == request.presentation_generation
    }

    fn with_native_picker_window<T>(
        &self,
        request: &NativePickerWindowRequest,
        still_current: impl Fn() -> bool,
        select: impl FnOnce(i64, &dyn Fn() -> Result<(), String>) -> Result<T, String>,
    ) -> Result<T, String> {
        // The field lease can query Host state. Invoke it outside HostInner's
        // lock, before and after validating the captured display.
        let verify = || {
            if !still_current()
                || !self.native_picker_window_is_current(request)
                || !still_current()
            {
                return Err(retired_picker());
            }
            Ok(())
        };
        verify()?;
        let result = select(request.source_hwnd as i64, &verify);
        verify()?;
        result
    }

    pub fn pick_folder_native_window_guarded(
        &self,
        params: &Value,
        request: &NativePickerWindowRequest,
        still_current: impl Fn() -> bool,
    ) -> Result<Value, String> {
        let initial = initial_path(params);
        self.with_native_picker_window(request, still_current, |owner, verify| {
            let path = crate::file_dialog_pick::pick_folder_guarded(
                initial.as_deref(), owner, &|| verify(),
            )?;
            Ok(json!({ "cancelled": path.is_none(), "path": path }))
        })
    }

    pub fn pick_files_native_window_guarded(
        &self,
        params: &Value,
        request: &NativePickerWindowRequest,
        still_current: impl Fn() -> bool,
    ) -> Result<Value, String> {
        let initial = initial_path(params);
        self.with_native_picker_window(request, still_current, |owner, verify| {
            let paths = crate::file_dialog_pick::pick_files_guarded(
                initial.as_deref(), owner, &|| verify(),
            )?;
            Ok(json!({ "cancelled": paths.is_empty(), "paths": paths }))
        })
    }

    pub fn pick_directories_native_window_guarded(
        &self,
        params: &Value,
        request: &NativePickerWindowRequest,
        still_current: impl Fn() -> bool,
    ) -> Result<Value, String> {
        let initial = initial_path(params);
        self.with_native_picker_window(request, still_current, |owner, verify| {
            let paths = crate::file_dialog_pick::pick_directories_guarded(
                initial.as_deref(), owner, &|| verify(),
            )?;
            Ok(json!({ "cancelled": paths.is_empty(), "paths": paths }))
        })
    }

    pub fn pick_image_file_native_window_guarded(
        &self,
        params: &Value,
        request: &NativePickerWindowRequest,
        still_current: impl Fn() -> bool,
    ) -> Result<Value, String> {
        let initial = initial_path(params);
        self.with_native_picker_window(request, still_current, |owner, verify| {
            let path = crate::file_dialog_pick::pick_image_file_guarded(
                initial.as_deref(), owner, &|| verify(),
            )?;
            verify()?;
            let Some(path) = path else {
                return Ok(json!({ "cancelled": true, "path": Value::Null }));
            };
            let root = self.inner.lock().paths.data_root.clone();
            verify()?;
            let staged = crate::window_background::stage_copy_guarded(
                &root, Path::new(&path), &|| verify(),
            );
            verify()?;
            let staged = staged?;
            Ok(json!({ "cancelled": false, "path": staged.to_string_lossy() }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _root: tempfile::TempDir,
        host: HostState,
    }

    fn fixture() -> Fixture {
        crate::clipboard::force_headless(true);
        let temporary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&temporary).unwrap();
        let root = tempfile::Builder::new().prefix("picker-window-").tempdir_in(temporary).unwrap();
        let host = HostState::open(AppPaths::from_data_root(root.path().join("data")), 29065).unwrap();
        {
            let mut inner = host.inner.lock();
            inner.host_hwnd = 101;
            inner.panel_hwnd = 202;
            inner.main_hwnd = 303;
            inner.window_visible = true;
            inner.ui.panel_visible = true;
            inner.ui.main_visible = true;
            inner.window_runtime_ready = true;
            inner.clipboard_web_view_ready = true;
            inner.panel_web_view_ready = true;
            for index in 0..3 {
                inner.chrome_input_epochs[index].window = index as u64 + 10;
                inner.chrome_input_epochs[index].list = index as u64 + 20;
                inner.chrome_presentation_epochs[index] = index as u64 + 30;
            }
        }
        Fixture { _root: root, host }
    }

    #[test]
    fn captures_registered_clipboard_panel_and_main_without_retargeting_the_picker() {
        let f = fixture();
        for (owner, surface) in [
            (101, PickerSurface::Clipboard),
            (202, PickerSurface::Panel),
            (303, PickerSurface::Main),
        ] {
            let request = f.host.capture_native_picker_window(owner).unwrap();
            assert_eq!(request.surface, surface);
            assert!(f.host.native_picker_window_is_current(&request));
            let selected = f.host.with_native_picker_window(&request, || true, |captured, verify| {
                assert_eq!(captured, owner as i64);
                verify()?;
                Ok("selected path")
            });
            assert_eq!(selected, Ok("selected path"));
        }
    }

    #[test]
    fn rejects_unknown_hidden_unready_and_ambiguously_registered_roots_at_acceptance() {
        let f = fixture();
        for owner in [0, 404] {
            assert!(f.host.capture_native_picker_window(owner).is_err());
        }
        {
            let mut inner = f.host.inner.lock();
            inner.window_visible = false;
            inner.ui.panel_visible = false;
            inner.ui.main_visible = false;
        }
        for owner in [101, 202, 303] {
            assert!(f.host.capture_native_picker_window(owner).is_err());
        }
        {
            let mut inner = f.host.inner.lock();
            inner.window_visible = true;
            inner.ui.panel_visible = true;
            inner.ui.main_visible = true;
            inner.clipboard_web_view_ready = false;
            inner.panel_web_view_ready = false;
        }
        assert!(f.host.capture_native_picker_window(101).is_err());
        assert!(f.host.capture_native_picker_window(202).is_err());
        {
            let mut inner = f.host.inner.lock();
            inner.window_runtime_ready = false;
        }
        assert!(f.host.capture_native_picker_window(303).is_err());
        {
            let mut inner = f.host.inner.lock();
            inner.window_runtime_ready = true;
            inner.panel_hwnd = 303;
        }
        assert!(f.host.capture_native_picker_window(303).is_err());
    }

    #[test]
    fn queued_picker_never_borrows_a_reopened_or_replaced_display() {
        for surface in [PickerSurface::Clipboard, PickerSurface::Panel, PickerSurface::Main] {
            for replacement in ["reopen", "hwnd", "window"] {
                let f = fixture();
                let owner = surface.state(&f.host.inner.lock()).0;
                let request = f.host.capture_native_picker_window(owner).unwrap();
                {
                    let mut inner = f.host.inner.lock();
                    let index = surface.index();
                    match replacement {
                        // Hide and Show can finish before the queued worker runs;
                        // the old request still carries the previous display.
                        "reopen" => {
                            match surface {
                                PickerSurface::Clipboard => inner.window_visible = false,
                                PickerSurface::Panel => inner.ui.panel_visible = false,
                                PickerSurface::Main => inner.ui.main_visible = false,
                            }
                            inner.chrome_presentation_epochs[index] += 1;
                            match surface {
                                PickerSurface::Clipboard => inner.window_visible = true,
                                PickerSurface::Panel => inner.ui.panel_visible = true,
                                PickerSurface::Main => inner.ui.main_visible = true,
                            }
                        }
                        "window" => inner.chrome_input_epochs[index].window += 1,
                        "hwnd" => match surface {
                            PickerSurface::Clipboard => inner.host_hwnd = 404,
                            PickerSurface::Panel => inner.panel_hwnd = 404,
                            PickerSurface::Main => inner.main_hwnd = 404,
                        },
                        _ => unreachable!(),
                    }
                }
                let result = f.host.with_native_picker_window(&request, || true, |_, _| {
                    panic!("retired queued request must not open a native picker")
                });
                assert_eq!(result, Err::<(), _>(retired_picker()), "{surface:?}/{replacement}");
            }
        }
    }

    #[test]
    fn focus_handoff_and_navigation_epoch_do_not_retire_a_visible_picker() {
        let f = fixture();
        for owner in [101, 202, 303] {
            let request = f.host.capture_native_picker_window(owner).unwrap();
            {
                let mut inner = f.host.inner.lock();
                inner.chrome_input_epochs[request.surface.index()].list += 1;
                inner.is_active = false;
                inner.has_host_fake_focus = false;
                inner.ui.panel_fake_focus = false;
                inner.last_fg = 909;
            }
            assert!(f.host.native_picker_window_is_current(&request));
        }
    }

    #[test]
    fn field_retirement_wins_over_selected_cancelled_and_failed_replies() {
        for outcome in [Ok(Some("picked")), Ok(None), Err("picker failed".to_string())] {
            let f = fixture();
            let request = f.host.capture_native_picker_window(202).unwrap();
            let active = AtomicBool::new(true);
            let result = f.host.with_native_picker_window(
                &request,
                || active.load(Ordering::SeqCst),
                |owner, verify| {
                    assert_eq!(owner, 202);
                    verify()?;
                    active.store(false, Ordering::SeqCst);
                    outcome
                },
            );
            assert_eq!(result, Err(retired_picker()));
        }
        let f = fixture();
        let request = f.host.capture_native_picker_window(202).unwrap();
        let another = fixture();
        assert!(!another.host.native_picker_window_is_current(&request));
        let result = another.host.with_native_picker_window(&request, || true, |_, _| {
            panic!("another Host cannot use this request")
        });
        assert_eq!(result, Err::<(), _>(retired_picker()));
    }

    #[test]
    fn runtime_shutdown_and_display_retirement_reject_success_cancel_and_error() {
        for retirement in ["runtime", "presentation"] {
            for outcome in [Ok(Some("picked")), Ok(None), Err("picker failed".to_string())] {
                let f = fixture();
                let request = f.host.capture_native_picker_window(202).unwrap();
                let result = f.host.with_native_picker_window(&request, || true, |_, verify| {
                    verify()?;
                    if retirement == "runtime" {
                        f.host.set_window_runtime_ready(false);
                    } else {
                        f.host.retire_window_presentation("panel");
                    }
                    outcome
                });
                assert_eq!(result, Err(retired_picker()), "{retirement}");
            }
        }
    }
}
