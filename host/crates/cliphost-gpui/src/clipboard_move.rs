//! HWND-local Snap suppression around the clipboard's native SC_MOVE loop.
//! No GPUI/App borrow is held while Windows dispatches its modal messages.

#[cfg(windows)]
mod win {
    use std::{
        cell::{Cell, RefCell},
        collections::HashMap,
    };
    use windows::Win32::{
        Foundation::{GetLastError, SetLastError, HWND, LPARAM, LRESULT, WIN32_ERROR, WPARAM},
        System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
        UI::{Shell::DefSubclassProc, WindowsAndMessaging::*},
    };

    const CONTROLLED: isize = (WS_THICKFRAME.0 | WS_MAXIMIZEBOX.0) as isize;

    struct Policy {
        generation: u64,
        resizable: bool,
        moving: bool,
        internal_writes: usize,
    }

    thread_local! {
        static POLICIES: RefCell<HashMap<isize, Policy>> = RefCell::new(HashMap::new());
        static NEXT_GENERATION: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn register(hwnd: HWND) -> Result<(), String> {
        unsafe {
            let mut pid = 0;
            if hwnd.0.is_null()
                || GetWindowThreadProcessId(hwnd, Some(&mut pid)) != GetCurrentThreadId()
                || pid != GetCurrentProcessId()
            {
                return Err("Clipboard movement policy requires an owned UI-thread window.".into());
            }
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            let generation = POLICIES.with(|policies| {
                let mut policies = policies.borrow_mut();
                let policy = policies.entry(hwnd.0 as isize).or_insert_with(|| Policy {
                    generation: NEXT_GENERATION.with(|next| {
                        let generation = next.get().wrapping_add(1).max(1);
                        next.set(generation);
                        generation
                    }),
                    resizable: style & WS_THICKFRAME.0 as isize != 0,
                    moving: false,
                    internal_writes: 0,
                });
                policy.generation
            });
            apply_policy(hwnd, generation)
        }
    }

    /// Fold/Unfold writes intent even while the native style is suppressed.
    pub(crate) fn set_resizable(hwnd: HWND, resizable: bool) -> Option<Result<(), String>> {
        let generation = POLICIES.with(|policies| {
            let mut policies = policies.borrow_mut();
            let policy = policies.get_mut(&(hwnd.0 as isize))?;
            policy.resizable = resizable;
            Some(policy.generation)
        })?;
        Some(unsafe { apply_policy(hwnd, generation) })
    }

    struct InternalWrite {
        hwnd: isize,
        generation: u64,
    }
    impl Drop for InternalWrite {
        fn drop(&mut self) {
            POLICIES.with(|policies| {
                if let Some(policy) = policies
                    .borrow_mut()
                    .get_mut(&self.hwnd)
                    .filter(|policy| policy.generation == self.generation)
                {
                    policy.internal_writes = policy.internal_writes.saturating_sub(1);
                }
            });
        }
    }

    fn is_current(hwnd: HWND, generation: u64) -> bool {
        POLICIES.with(|policies| {
            policies
                .borrow()
                .get(&(hwnd.0 as isize))
                .is_some_and(|policy| policy.generation == generation)
        })
    }

    unsafe fn apply_policy(hwnd: HWND, generation: u64) -> Result<(), String> {
        let desired = POLICIES.with(|policies| {
            let mut policies = policies.borrow_mut();
            let policy = policies
                .get_mut(&(hwnd.0 as isize))
                .filter(|policy| policy.generation == generation)?;
            let desired = if policy.resizable && !policy.moving {
                WS_THICKFRAME.0 as isize
            } else {
                0
            };
            policy.internal_writes += 1;
            Some(desired)
        });
        let Some(desired) = desired else {
            return Ok(());
        };
        let _write = InternalWrite {
            hwnd: hwnd.0 as isize,
            generation,
        };
        let old = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let style = (old & !CONTROLLED) | desired;
        if style == old {
            return Ok(());
        }
        SetLastError(WIN32_ERROR(0));
        let previous = SetWindowLongPtrW(hwnd, GWL_STYLE, style);
        let error = GetLastError();
        // Both native calls can re-enter the HWND and destroy/replace its
        // registration. Never continue onto a reused handle or report a stale
        // owner's error into its successor's dispatch queue.
        if !is_current(hwnd, generation) {
            return Ok(());
        }
        if previous == 0 && error.0 != 0 {
            return Err(format!("Cannot update clipboard move style: {error:?}"));
        }
        let result = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        if !is_current(hwnd, generation) {
            return Ok(());
        }
        result.map_err(|error| format!("Cannot apply clipboard move frame: {error}"))
    }

    struct MoveScope {
        hwnd: HWND,
        generation: u64,
        armed: bool,
    }
    impl MoveScope {
        unsafe fn begin(hwnd: HWND) -> Result<Self, String> {
            let generation = POLICIES.with(|policies| {
                let mut policies = policies.borrow_mut();
                let policy = policies
                    .get_mut(&(hwnd.0 as isize))
                    .expect("registered clipboard");
                policy.moving = true;
                policy.generation
            });
            let scope = Self {
                hwnd,
                generation,
                armed: true,
            };
            apply_policy(hwnd, generation)?;
            Ok(scope)
        }

        unsafe fn restore(&self) -> Result<(), String> {
            let current = POLICIES.with(|policies| {
                let mut policies = policies.borrow_mut();
                let Some(policy) = policies
                    .get_mut(&(self.hwnd.0 as isize))
                    .filter(|policy| policy.generation == self.generation)
                else {
                    return false;
                };
                // Drop the enforcer before writing the current resize intent.
                policy.moving = false;
                true
            });
            if current {
                apply_policy(self.hwnd, self.generation)?;
            }
            Ok(())
        }

        unsafe fn finish(mut self) -> Result<(), String> {
            self.armed = false;
            self.restore()
        }
    }
    impl Drop for MoveScope {
        fn drop(&mut self) {
            if self.armed {
                if let Err(error) = unsafe { self.restore() } {
                    eprintln!("{error}");
                }
            }
        }
    }

    /// The caller continues its existing subclass chain for unhandled messages.
    pub(crate) unsafe fn handle_message(
        hwnd: HWND,
        message: u32,
        wp: WPARAM,
        lp: LPARAM,
    ) -> Option<Result<LRESULT, String>> {
        if message == WM_NCDESTROY {
            POLICIES.with(|policies| {
                policies.borrow_mut().remove(&(hwnd.0 as isize));
            });
            return None;
        }
        let moving = POLICIES
            .with(|policies| policies.borrow().get(&(hwnd.0 as isize)).map(|p| p.moving))?;
        if message == WM_STYLECHANGING && wp.0 as isize == GWL_STYLE.0 as isize && lp.0 != 0 {
            let styles = &mut *(lp.0 as *mut STYLESTRUCT);
            POLICIES.with(|policies| {
                let mut policies = policies.borrow_mut();
                let policy = policies
                    .get_mut(&(hwnd.0 as isize))
                    .expect("registered clipboard");
                if policy.internal_writes == 0
                    && (styles.styleOld ^ styles.styleNew) & WS_THICKFRAME.0 != 0
                {
                    policy.resizable = styles.styleNew & WS_THICKFRAME.0 != 0;
                }
                styles.styleNew &= !WS_MAXIMIZEBOX.0;
                if policy.moving || !policy.resizable {
                    styles.styleNew &= !WS_THICKFRAME.0;
                } else {
                    styles.styleNew |= WS_THICKFRAME.0;
                }
            });
        }
        if message != WM_SYSCOMMAND {
            return None;
        }
        match wp.0 as u32 & 0xfff0 {
            SC_MAXIMIZE => Some(Ok(LRESULT(0))),
            SC_MOVE | SC_SIZE if moving => Some(Ok(LRESULT(0))),
            SC_MOVE => Some((|| {
                let scope = MoveScope::begin(hwnd)?;
                if !is_current(hwnd, scope.generation) {
                    return Ok(LRESULT(0));
                }
                // Keep suppression until DefWindowProc's entire modal call
                // returns, including release/cancel and the final snap action.
                let result = DefSubclassProc(hwnd, message, wp, lp);
                scope.finish()?;
                Ok(result)
            })()),
            _ => None,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::Once;
        use windows::{
            core::w,
            Win32::{
                Foundation::{HINSTANCE, RECT},
                System::LibraryLoader::GetModuleHandleW,
                UI::Shell::{RemoveWindowSubclass, SetWindowSubclass},
            },
        };

        #[derive(Clone, Copy, Default)]
        enum Action {
            #[default]
            Return,
            NestedAndCancel,
            ResizeIntent(bool),
            Destroy,
        }
        #[derive(Default)]
        struct Probe {
            action: Cell<Action>,
            moves: Cell<usize>,
            sizes: Cell<usize>,
            maximizes: Cell<usize>,
            inside_style: Cell<isize>,
            after_exit_style: Cell<isize>,
            wp: Cell<usize>,
            lp: Cell<isize>,
            nested_move: Cell<isize>,
            nested_size: Cell<isize>,
            enters: Cell<usize>,
            exits: Cell<usize>,
            error: RefCell<Option<String>>,
        }

        unsafe fn send(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            SendMessageW(hwnd, msg, Some(wp), Some(lp))
        }

        unsafe extern "system" fn base_proc(
            hwnd: HWND,
            msg: u32,
            wp: WPARAM,
            lp: LPARAM,
        ) -> LRESULT {
            // A hidden custom-frame HWND, with a fixed client inset independent
            // of WS_THICKFRAME, as required by GPUI's frame calculation.
            if msg == WM_NCCALCSIZE && wp.0 != 0 {
                return LRESULT(0);
            }
            let probe = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Probe).as_ref();
            if let Some(probe) = probe {
                if msg == WM_ENTERSIZEMOVE {
                    probe.enters.set(probe.enters.get() + 1);
                    return LRESULT(0);
                }
                if msg == WM_EXITSIZEMOVE {
                    probe.exits.set(probe.exits.get() + 1);
                    return LRESULT(0);
                }
                if msg == WM_SYSCOMMAND {
                    match wp.0 as u32 & 0xfff0 {
                        SC_MOVE => {
                            probe.moves.set(probe.moves.get() + 1);
                            probe.inside_style.set(GetWindowLongPtrW(hwnd, GWL_STYLE));
                            probe.wp.set(wp.0);
                            probe.lp.set(lp.0);
                            match probe.action.get() {
                                Action::Return => {}
                                Action::NestedAndCancel => {
                                    send(hwnd, WM_ENTERSIZEMOVE, WPARAM(0), LPARAM(0));
                                    probe.nested_move.set(
                                        send(
                                            hwnd,
                                            WM_SYSCOMMAND,
                                            WPARAM(SC_MOVE as usize),
                                            LPARAM(0),
                                        )
                                        .0,
                                    );
                                    probe.nested_size.set(
                                        send(
                                            hwnd,
                                            WM_SYSCOMMAND,
                                            WPARAM(SC_SIZE as usize),
                                            LPARAM(0),
                                        )
                                        .0,
                                    );
                                    send(hwnd, WM_CANCELMODE, WPARAM(0), LPARAM(0));
                                    send(hwnd, WM_CAPTURECHANGED, WPARAM(0), LPARAM(0));
                                    send(hwnd, WM_EXITSIZEMOVE, WPARAM(0), LPARAM(0));
                                    probe
                                        .after_exit_style
                                        .set(GetWindowLongPtrW(hwnd, GWL_STYLE));
                                }
                                Action::ResizeIntent(resizable) => {
                                    // An external style writer cannot re-enable
                                    // Snap during the modal call, but unrelated
                                    // changes must survive restoration.
                                    let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
                                    SetWindowLongPtrW(
                                        hwnd,
                                        GWL_STYLE,
                                        (style | CONTROLLED | WS_DISABLED.0 as isize)
                                            & !(WS_MINIMIZEBOX.0 as isize),
                                    );
                                    if let Some(Err(error)) = set_resizable(hwnd, resizable) {
                                        *probe.error.borrow_mut() = Some(error);
                                    }
                                    probe
                                        .after_exit_style
                                        .set(GetWindowLongPtrW(hwnd, GWL_STYLE));
                                }
                                Action::Destroy => {
                                    if let Err(error) = DestroyWindow(hwnd) {
                                        *probe.error.borrow_mut() = Some(error.to_string());
                                    }
                                }
                            }
                            // Deliberately return without entering a desktop
                            // modal loop. The real HWND/subclass/style paths run.
                            return LRESULT(77);
                        }
                        SC_SIZE => {
                            probe.sizes.set(probe.sizes.get() + 1);
                            return LRESULT(43);
                        }
                        SC_MAXIMIZE => {
                            probe.maximizes.set(probe.maximizes.get() + 1);
                            return LRESULT(44);
                        }
                        _ => {}
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }

        unsafe extern "system" fn subclass(
            hwnd: HWND,
            msg: u32,
            wp: WPARAM,
            lp: LPARAM,
            id: usize,
            _: usize,
        ) -> LRESULT {
            if let Some(result) = handle_message(hwnd, msg, wp, lp) {
                return match result {
                    Ok(result) => result,
                    Err(error) => {
                        if let Some(probe) =
                            (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Probe).as_ref()
                        {
                            *probe.error.borrow_mut() = Some(error);
                        }
                        LRESULT(-1)
                    }
                };
            }
            if msg == WM_NCDESTROY {
                let _ = RemoveWindowSubclass(hwnd, Some(subclass), id);
            }
            DefSubclassProc(hwnd, msg, wp, lp)
        }

        struct Owned {
            hwnd: HWND,
            probe: Box<Probe>,
        }
        impl Owned {
            fn new(registered: bool) -> Self {
                static CLASS: Once = Once::new();
                unsafe {
                    let instance = HINSTANCE(GetModuleHandleW(None).unwrap().0);
                    CLASS.call_once(|| {
                        assert_ne!(
                            RegisterClassW(&WNDCLASSW {
                                lpfnWndProc: Some(base_proc),
                                hInstance: instance,
                                lpszClassName: w!("ClipHostClipboardMoveScopeFixture"),
                                ..Default::default()
                            }),
                            0
                        );
                    });
                    let hwnd = CreateWindowExW(
                        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                        w!("ClipHostClipboardMoveScopeFixture"),
                        w!("Hidden clipboard move regression"),
                        WS_POPUP | WS_THICKFRAME | WS_MAXIMIZEBOX | WS_MINIMIZEBOX,
                        -32000,
                        -32000,
                        360,
                        280,
                        None,
                        None,
                        Some(instance),
                        None,
                    )
                    .unwrap();
                    let probe = Box::<Probe>::default();
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, probe.as_ref() as *const Probe as isize);
                    assert!(SetWindowSubclass(hwnd, Some(subclass), 0xBC99, 0).as_bool());
                    if registered {
                        register(hwnd).unwrap();
                    }
                    Self { hwnd, probe }
                }
            }
            fn style(&self) -> isize {
                unsafe { GetWindowLongPtrW(self.hwnd, GWL_STYLE) }
            }
            fn command(&self, command: u32) -> isize {
                unsafe {
                    send(
                        self.hwnd,
                        WM_SYSCOMMAND,
                        WPARAM(command as usize),
                        LPARAM(123456),
                    )
                    .0
                }
            }
            fn geometry(&self) -> (i32, i32, i32, i32, i32, i32) {
                let mut outer = RECT::default();
                let mut client = RECT::default();
                unsafe {
                    GetWindowRect(self.hwnd, &mut outer).unwrap();
                    GetClientRect(self.hwnd, &mut client).unwrap();
                }
                (
                    outer.left,
                    outer.top,
                    outer.right - outer.left,
                    outer.bottom - outer.top,
                    client.right - client.left,
                    client.bottom - client.top,
                )
            }
        }
        impl Drop for Owned {
            fn drop(&mut self) {
                unsafe {
                    if IsWindow(Some(self.hwnd)).as_bool() {
                        let _ = DestroyWindow(self.hwnd);
                    }
                }
            }
        }

        #[test]
        fn native_move_masks_style_and_restores_after_a_non_modal_return_without_resizing() {
            let window = Owned::new(true);
            let geometry = window.geometry();
            assert_eq!(window.style() & CONTROLLED, WS_THICKFRAME.0 as isize);
            assert_eq!(window.command(SC_MOVE | 2), 77);
            assert_eq!(window.probe.wp.get(), (SC_MOVE | 2) as usize);
            assert_eq!(window.probe.lp.get(), 123456);
            assert_eq!(window.probe.inside_style.get() & CONTROLLED, 0);
            assert_eq!(window.style() & CONTROLLED, WS_THICKFRAME.0 as isize);
            assert_eq!(window.geometry(), geometry);
            assert!(window.probe.error.borrow().is_none());
        }

        #[test]
        fn nested_commands_and_exit_cancel_notifications_do_not_restore_before_move_returns() {
            let window = Owned::new(true);
            window.probe.action.set(Action::NestedAndCancel);
            assert_eq!(window.command(SC_MOVE), 77);
            assert_eq!(window.probe.moves.get(), 1);
            assert_eq!(window.probe.sizes.get(), 0);
            assert_eq!(
                (window.probe.enters.get(), window.probe.exits.get()),
                (1, 1)
            );
            assert_eq!(
                (
                    window.probe.nested_move.get(),
                    window.probe.nested_size.get()
                ),
                (0, 0)
            );
            assert_eq!(window.probe.after_exit_style.get() & CONTROLLED, 0);
            assert_eq!(window.style() & CONTROLLED, WS_THICKFRAME.0 as isize);
            assert_eq!(
                window.command(SC_SIZE | 1),
                43,
                "ordinary border sizing stays native"
            );
            assert_eq!(window.probe.sizes.get(), 1);
        }

        #[test]
        fn fold_intent_and_external_style_writes_keep_the_latest_policy_and_unrelated_bits() {
            let window = Owned::new(true);
            assert_ne!(window.style() & WS_MINIMIZEBOX.0 as isize, 0);
            for resizable in [false, true] {
                window.probe.action.set(Action::ResizeIntent(resizable));
                assert_eq!(window.command(SC_MOVE), 77);
                assert_eq!(window.probe.after_exit_style.get() & CONTROLLED, 0);
                assert_eq!(
                    window.style() & CONTROLLED,
                    if resizable {
                        WS_THICKFRAME.0 as isize
                    } else {
                        0
                    }
                );
                assert_ne!(window.style() & WS_DISABLED.0 as isize, 0);
                assert_eq!(window.style() & WS_MINIMIZEBOX.0 as isize, 0);
            }
            let style = window.style();
            unsafe {
                SetWindowLongPtrW(window.hwnd, GWL_STYLE, style | WS_MAXIMIZEBOX.0 as isize);
            }
            assert_eq!(window.style() & WS_MAXIMIZEBOX.0 as isize, 0);
            assert_eq!(window.command(SC_MAXIMIZE), 0);
            assert_eq!(window.probe.maximizes.get(), 0);
            assert!(window.probe.error.borrow().is_none());
        }

        #[test]
        fn destroyed_move_owner_is_retired_and_an_unregistered_window_is_unchanged() {
            let other = Owned::new(false);
            assert_eq!(other.command(SC_MOVE), 77);
            assert_eq!(other.probe.inside_style.get() & CONTROLLED, CONTROLLED);
            assert_eq!(other.command(SC_MAXIMIZE), 44);
            let window = Owned::new(true);
            window.probe.action.set(Action::Destroy);
            assert_eq!(window.command(SC_MOVE), 77);
            assert!(!unsafe { IsWindow(Some(window.hwnd)).as_bool() });
            assert!(!POLICIES
                .with(|policies| policies.borrow().contains_key(&(window.hwnd.0 as isize))));
            assert!(window.probe.error.borrow().is_none());
        }
    }
}

#[cfg(windows)]
pub(crate) use win::{handle_message, register, set_resizable};
