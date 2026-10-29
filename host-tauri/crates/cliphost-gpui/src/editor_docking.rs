//! A root-HWND-only editor companion. Position messages merely queue work:
//! SetWindowPos must run after any GPUI App/Window borrow has been released.

#[cfg(windows)]
mod win {
    use std::{
        cell::{Cell, RefCell},
        collections::HashMap,
    };
    use windows::Win32::{
        Foundation::{
            GetLastError, SetLastError, HWND, LPARAM, LRESULT, RECT, WIN32_ERROR, WPARAM,
        },
        Graphics::{
            Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS},
            Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST},
        },
        System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
        UI::{
            HiDpi::GetDpiForWindow,
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    };

    const SUBCLASS: usize = 0x434c_4544;
    const SYNC: u32 = WM_APP + 0x139;
    const RESTORE: u32 = WM_APP + 0x13a;

    #[derive(Clone, Copy)]
    struct Dock {
        editor: isize,
        owner: isize,
        generation: usize,
        preferred_width_dip: f64,
        minimum_width_dip: f64,
        enabled: bool,
        pending: bool,
        syncing: bool,
        reveal: bool,
        restore_owner: Option<BoxRect>,
        // Automatic room-making and restoration stop as soon as the owner is
        // moved externally. The native move loop must remain its sole writer.
        owner_untouched: bool,
        last_owner: BoxRect,
    }

    #[derive(Clone, Copy)]
    struct Restore {
        generation: usize,
        original: BoxRect,
        expected: BoxRect,
    }

    thread_local! {
        static DOCKS: RefCell<HashMap<isize, Dock>> = RefCell::new(HashMap::new());
        static RESTORES: RefCell<HashMap<isize, Restore>> = RefCell::new(HashMap::new());
        // Insets belong to the HWND, not a docking session. A retained LAN
        // window is unregistered while hidden and may share its slot with the
        // editor before being opened again.
        static FRAME_INSETS: RefCell<HashMap<isize, FrameInsets>> = RefCell::new(HashMap::new());
        static NEXT: Cell<usize> = const { Cell::new(0) };
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct BoxRect {
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    }

    impl From<RECT> for BoxRect {
        fn from(rect: RECT) -> Self {
            Self {
                left: rect.left,
                top: rect.top,
                width: rect.right.saturating_sub(rect.left),
                height: rect.bottom.saturating_sub(rect.top),
            }
        }
    }

    #[derive(Clone, Copy)]
    struct FrameInsets {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        dpi: u32,
    }

    impl FrameInsets {
        fn measured(outer: BoxRect, visible: BoxRect, dpi: u32) -> Self {
            Self {
                left: visible.left - outer.left,
                top: visible.top - outer.top,
                right: outer.width - visible.width - (visible.left - outer.left),
                bottom: outer.height - visible.height - (visible.top - outer.top),
                dpi,
            }
        }

        fn visible(self, outer: BoxRect, dpi: u32) -> BoxRect {
            let scale = f64::from(dpi.max(96)) / f64::from(self.dpi.max(96));
            let scaled = |inset| (f64::from(inset) * scale).round() as i32;
            let left = scaled(self.left);
            let top = scaled(self.top);
            BoxRect {
                left: outer.left + left,
                top: outer.top + top,
                width: (outer.width - left - scaled(self.right)).max(1),
                height: (outer.height - top - scaled(self.bottom)).max(1),
            }
        }
    }

    /// The permanent native window dispatcher calls this even after undocking.
    pub(crate) fn forget_frame(hwnd: HWND) {
        FRAME_INSETS.with(|frames| frames.borrow_mut().remove(&(hwnd.0 as isize)));
    }

    /// Preserve the requested side and height. On opening, make room for the
    /// pair if necessary. Once the owner has moved, follow its actual position
    /// without pulling it back into this monitor's work area.
    fn placement(
        owner: BoxRect,
        preferred_width: i32,
        minimum_width: i32,
        work: BoxRect,
        allow_owner_shift: bool,
    ) -> (i32, BoxRect) {
        let work_right = i64::from(work.left) + i64::from(work.width.max(1));
        let minimum_width = i64::from(minimum_width.max(1).min(work.width.max(1)));
        let width = i64::from(preferred_width.max(1))
            .max(minimum_width)
            .min((i64::from(work.width) - i64::from(owner.width)).max(minimum_width));
        let max_owner_left = work_right - i64::from(owner.width) - width;
        let owner_left = if !allow_owner_shift {
            i64::from(owner.left)
        } else if max_owner_left >= i64::from(work.left) {
            i64::from(owner.left).clamp(i64::from(work.left), max_owner_left)
        } else {
            max_owner_left
        };
        let x = owner_left.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        (
            x,
            BoxRect {
                left: (owner_left + i64::from(owner.width))
                    .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
                top: owner.top,
                width: width as i32,
                height: owner.height,
            },
        )
    }

    unsafe fn minimum_width(owner: HWND, work: BoxRect, dip: f64) -> i32 {
        ((dip * f64::from(GetDpiForWindow(owner).max(96)) / 96.0).round() as i32)
            .min((work.width / 2).max(1))
    }

    unsafe fn owned_root(hwnd: HWND) -> bool {
        let mut pid = 0;
        !hwnd.0.is_null()
            && GetWindowThreadProcessId(hwnd, Some(&mut pid)) == GetCurrentThreadId()
            && pid == GetCurrentProcessId()
            && GetWindowLongPtrW(hwnd, GWL_STYLE) & WS_CHILD.0 as isize == 0
    }

    fn current(editor: isize, generation: usize) -> Option<Dock> {
        DOCKS.with(|docks| {
            docks
                .borrow()
                .get(&editor)
                .filter(|dock| dock.generation == generation)
                .copied()
        })
    }

    fn relation(hwnd: HWND, generation: usize) -> Option<Dock> {
        DOCKS.with(|docks| {
            docks
                .borrow()
                .values()
                .find(|dock| {
                    dock.generation == generation
                        && (dock.editor == hwnd.0 as isize || dock.owner == hwnd.0 as isize)
                })
                .copied()
        })
    }

    unsafe fn frame(hwnd: HWND) -> Result<(BoxRect, BoxRect), String> {
        let mut outer = RECT::default();
        GetWindowRect(hwnd, &mut outer).map_err(|error| error.to_string())?;
        let dpi = GetDpiForWindow(hwnd).max(96);
        let mut visible = RECT::default();
        let visible = if IsWindowVisible(hwnd).as_bool()
            && DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut visible as *mut _ as *mut _,
                std::mem::size_of::<RECT>() as u32,
            )
            .is_ok()
            && visible.right > visible.left
            && visible.bottom > visible.top
            && visible.left >= outer.left
            && visible.top >= outer.top
            && visible.right <= outer.right
            && visible.bottom <= outer.bottom
        {
            let insets = FrameInsets::measured(outer.into(), visible.into(), dpi);
            FRAME_INSETS.with(|frames| frames.borrow_mut().insert(hwnd.0 as isize, insets));
            visible
        } else {
            // DWM bounds for hidden windows can be stale. Rebuild from the
            // current outer rectangle instead; treating it as the visible
            // width here would add the resize margins again on every reopen.
            return Ok((outer.into(), FRAME_INSETS.with(|frames| {
                frames.borrow().get(&(hwnd.0 as isize))
                    .map_or_else(|| outer.into(), |insets| insets.visible(outer.into(), dpi))
            })));
        };
        Ok((outer.into(), visible.into()))
    }

    unsafe fn work_area(owner: HWND) -> Result<BoxRect, String> {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..MONITORINFO::default()
        };
        if !GetMonitorInfoW(
            MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST),
            &mut info,
        )
        .as_bool()
        {
            return Err("Cannot read the editor companion's monitor work area.".into());
        }
        Ok(info.rcWork.into())
    }

    unsafe fn remove(editor: isize, generation: usize, restore_owner: bool) {
        let dock = DOCKS.with(|docks| {
            let mut docks = docks.borrow_mut();
            if docks
                .get(&editor)
                .is_some_and(|dock| dock.generation == generation)
            {
                docks.remove(&editor)
            } else {
                None
            }
        });
        if let Some(dock) = dock {
            let _ = RemoveWindowSubclass(HWND(dock.editor as *mut _), Some(proc), SUBCLASS);
            let owner = HWND(dock.owner as *mut _);
            if restore_owner && dock.owner_untouched && owned_root(owner) {
                if let Some(original) = dock.restore_owner {
                    RESTORES.with(|restores| {
                        restores.borrow_mut().insert(
                            dock.owner,
                            Restore {
                                generation,
                                original,
                                expected: dock.last_owner,
                            },
                        )
                    });
                    if PostMessageW(Some(owner), RESTORE, WPARAM(generation), LPARAM(0)).is_ok() {
                        return;
                    }
                    RESTORES.with(|restores| restores.borrow_mut().remove(&dock.owner));
                }
            }
            let _ = RemoveWindowSubclass(owner, Some(proc), SUBCLASS);
        }
    }

    unsafe fn queue(dock: Dock) {
        let post = DOCKS.with(|docks| {
            let mut docks = docks.borrow_mut();
            let Some(current) = docks
                .get_mut(&dock.editor)
                .filter(|current| current.generation == dock.generation)
            else {
                return false;
            };
            if current.pending || current.syncing {
                return false;
            }
            current.pending = true;
            true
        });
        if post
            && PostMessageW(
                Some(HWND(dock.editor as *mut _)),
                SYNC,
                WPARAM(dock.generation),
                LPARAM(0),
            )
            .is_err()
        {
            DOCKS.with(|docks| {
                if let Some(current) = docks
                    .borrow_mut()
                    .get_mut(&dock.editor)
                    .filter(|current| current.generation == dock.generation)
                {
                    current.pending = false;
                }
            });
        }
    }

    struct SyncScope {
        editor: isize,
        generation: usize,
    }
    impl Drop for SyncScope {
        fn drop(&mut self) {
            DOCKS.with(|docks| {
                if let Some(dock) = docks
                    .borrow_mut()
                    .get_mut(&self.editor)
                    .filter(|dock| dock.generation == self.generation)
                {
                    dock.syncing = false;
                }
            });
        }
    }

    unsafe fn synchronize(editor: isize, generation: usize) -> Result<(), String> {
        let dock = DOCKS.with(|docks| {
            let mut docks = docks.borrow_mut();
            let dock = docks
                .get_mut(&editor)
                .filter(|dock| dock.generation == generation)?;
            dock.pending = false;
            if dock.syncing {
                return None;
            }
            dock.syncing = true;
            Some(*dock)
        });
        let Some(dock) = dock else {
            return Ok(());
        };
        let _scope = SyncScope { editor, generation };
        let owner = HWND(dock.owner as *mut _);
        let editor_hwnd = HWND(editor as *mut _);
        if !owned_root(owner) || !owned_root(editor_hwnd) {
            remove(editor, generation, false);
            return Ok(());
        }
        if !dock.enabled || !IsWindowVisible(owner).as_bool() || IsIconic(owner).as_bool() {
            if IsWindowVisible(editor_hwnd).as_bool()
                || GetWindowLongPtrW(editor_hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize != 0
            {
                // Release the companion's layer while preserving a still-visible
                // owner's pin (e.g. when the clipboard folds).
                super::super::raw_hide(editor_hwnd)?;
            }
            DOCKS.with(|docks| {
                if let Some(dock) = docks
                    .borrow_mut()
                    .get_mut(&editor)
                    .filter(|dock| dock.generation == generation)
                {
                    dock.reveal = true;
                }
            });
        }
        if current(editor, generation).is_none() {
            return Ok(());
        }
        let (outer, visible) = frame(owner)?;
        let (editor_outer, editor_visible) = frame(editor_hwnd)?;
        let work = work_area(owner)?;
        let preferred_width = (dock.preferred_width_dip * f64::from(GetDpiForWindow(owner).max(96))
            / 96.0)
            .round() as i32;
        let (owner_left, target) = placement(
            visible,
            preferred_width,
            minimum_width(owner, work, dock.minimum_width_dip),
            work,
            dock.owner_untouched && dock.enabled,
        );
        if owner_left != visible.left {
            DOCKS.with(|docks| {
                if let Some(dock) = docks
                    .borrow_mut()
                    .get_mut(&editor)
                    .filter(|dock| dock.generation == generation && dock.owner_untouched)
                {
                    dock.restore_owner.get_or_insert(outer);
                }
            });
            SetWindowPos(
                owner,
                None,
                owner_left - (visible.left - outer.left),
                outer.top,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            )
            .map_err(|error| error.to_string())?;
        }
        if current(editor, generation).is_none() {
            return Ok(());
        }
        let last_owner = frame(owner)?.0;
        DOCKS.with(|docks| {
            if let Some(dock) = docks
                .borrow_mut()
                .get_mut(&editor)
                .filter(|dock| dock.generation == generation)
            {
                dock.last_owner = last_owner;
            }
        });
        // Account for DWM's invisible resize margins so the painted surfaces
        // touch, even though their HWND rectangles may slightly overlap.
        let target_outer = BoxRect {
            left: target.left - (editor_visible.left - editor_outer.left),
            top: target.top - (editor_visible.top - editor_outer.top),
            width: target.width + editor_outer.width - editor_visible.width,
            height: target.height + editor_outer.height - editor_visible.height,
        };
        let reveal = dock.enabled && dock.reveal && IsWindowVisible(owner).as_bool() && !IsIconic(owner).as_bool();
        let mut flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
        let mut layer = None;
        if reveal {
            // Hide releases TOPMOST. Restore the companion's current owner
            // layer only on reveal; geometry maintenance must not reorder it.
            flags &= !SWP_NOZORDER;
            flags |= SWP_SHOWWINDOW;
            layer = Some(if GetWindowLongPtrW(owner, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize != 0 {
                HWND_TOPMOST
            } else {
                HWND_NOTOPMOST
            });
        }
        if target_outer == editor_outer {
            flags |= SWP_NOMOVE | SWP_NOSIZE;
        }
        let was_visible = IsWindowVisible(editor_hwnd).as_bool();
        SetWindowPos(
            editor_hwnd,
            layer,
            target_outer.left,
            target_outer.top,
            target_outer.width.max(1),
            target_outer.height.max(1),
            flags,
        )
        .map_err(|error| error.to_string())?;
        if reveal {
            DOCKS.with(|docks| {
                if let Some(dock) = docks
                    .borrow_mut()
                    .get_mut(&editor)
                    .filter(|dock| dock.generation == generation)
                {
                    dock.reveal = false;
                }
            });
        }
        // A never-shown HWND may only acquire its DWM frame margins once it is
        // visible. Refine that initial placement on a second posted dispatch.
        drop(_scope);
        if reveal && !was_visible {
            queue(dock);
        }
        Ok(())
    }

    /// Called only by platform's queued native dispatcher, never under GPUI.
    pub(crate) fn register(editor: HWND, owner: isize) -> Result<(), String> {
        register_with_minimum(editor, owner, f64::from(crate::editor::minimum_width_dip()))
    }

    pub(crate) fn register_with_minimum(editor: HWND, owner: isize, minimum_width_dip: f64) -> Result<(), String> {
        let owner = HWND(owner as *mut _);
        unsafe {
            if editor == owner || !owned_root(editor) || !owned_root(owner) {
                return Err(
                    "Editor docking requires two owned root windows on this UI thread.".into(),
                );
            }
            let existing = DOCKS.with(|docks| {
                let mut docks = docks.borrow_mut();
                let dock = docks
                    .get_mut(&(editor.0 as isize))
                    .filter(|dock| dock.owner == owner.0 as isize)?;
                dock.reveal = true;
                dock.enabled = true;
                dock.minimum_width_dip = minimum_width_dip.max(1.);
                Some(*dock)
            });
            if let Some(dock) = existing {
                return synchronize(dock.editor, dock.generation);
            }
            let old = DOCKS.with(|docks| {
                docks
                    .borrow()
                    .values()
                    .filter(|dock| {
                        dock.editor == editor.0 as isize || dock.owner == owner.0 as isize
                    })
                    .copied()
                    .collect::<Vec<_>>()
            });
            for old in old {
                remove(old.editor, old.generation, false);
            }
            if RESTORES
                .with(|restores| restores.borrow_mut().remove(&(owner.0 as isize)))
                .is_some()
            {
                let _ = RemoveWindowSubclass(owner, Some(proc), SUBCLASS);
            }
            let generation = NEXT.with(|next| {
                let value = next.get().wrapping_add(1).max(1);
                next.set(value);
                value
            });
            let (_, visible) = frame(editor)?;
            let dock = Dock {
                editor: editor.0 as isize,
                owner: owner.0 as isize,
                generation,
                preferred_width_dip: f64::from(visible.width.max(1)) * 96.0
                    / f64::from(GetDpiForWindow(editor).max(96)),
                minimum_width_dip: minimum_width_dip.max(1.),
                enabled: true,
                pending: false,
                syncing: true,
                reveal: true,
                restore_owner: None,
                owner_untouched: true,
                last_owner: frame(owner)?.0,
            };
            DOCKS.with(|docks| docks.borrow_mut().insert(dock.editor, dock));
            if !SetWindowSubclass(editor, Some(proc), SUBCLASS, generation).as_bool()
                || !SetWindowSubclass(owner, Some(proc), SUBCLASS, generation).as_bool()
            {
                remove(dock.editor, generation, false);
                return Err("Cannot register the editor companion's window handlers.".into());
            }
            let configure = (|| {
                SetLastError(WIN32_ERROR(0));
                if SetWindowLongPtrW(editor, GWLP_HWNDPARENT, owner.0 as isize) == 0
                    && GetLastError().0 != 0
                {
                    return Err(format!(
                        "Cannot assign the editor owner: {:?}",
                        GetLastError()
                    ));
                }
                if current(dock.editor, generation).is_none() {
                    return Ok(());
                }
                let old = GetWindowLongPtrW(editor, GWL_EXSTYLE);
                let style = (old | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_APPWINDOW.0 as isize);
                if style != old {
                    SetLastError(WIN32_ERROR(0));
                    if SetWindowLongPtrW(editor, GWL_EXSTYLE, style) == 0 && GetLastError().0 != 0 {
                        return Err(format!(
                            "Cannot configure the editor tool window: {:?}",
                            GetLastError()
                        ));
                    }
                    if current(dock.editor, generation).is_some() {
                        SetWindowPos(
                            editor,
                            None,
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE
                                | SWP_NOSIZE
                                | SWP_NOZORDER
                                | SWP_NOACTIVATE
                                | SWP_NOOWNERZORDER
                                | SWP_FRAMECHANGED,
                        )
                        .map_err(|error| error.to_string())?;
                    }
                }
                Ok(())
            })();
            if let Err(error) = configure {
                remove(dock.editor, generation, false);
                return Err(error);
            }
            DOCKS.with(|docks| {
                if let Some(dock) = docks
                    .borrow_mut()
                    .get_mut(&dock.editor)
                    .filter(|dock| dock.generation == generation)
                {
                    dock.syncing = false;
                }
            });
            synchronize(dock.editor, generation)
        }
    }

    /// A dismissed pane keeps its GPUI entity but must stop following its owner.
    pub(crate) fn unregister(editor: HWND) -> Result<(), String> {
        let dock = DOCKS.with(|docks| docks.borrow().get(&(editor.0 as isize)).copied());
        if let Some(dock) = dock {
            unsafe { remove(dock.editor, dock.generation, true); }
        }
        Ok(())
    }

    /// Fold keeps the owner HWND visible; suspend only its companion until the
    /// full clipboard returns. An ordinary Hide still follows native ownership.
    pub(crate) fn set_enabled(editor: HWND, enabled: bool) -> Result<(), String> {
        let dock = DOCKS.with(|docks| {
            let mut docks = docks.borrow_mut();
            let dock = docks.get_mut(&(editor.0 as isize))?;
            if dock.enabled == enabled { return None; }
            dock.enabled = enabled;
            dock.reveal = true;
            Some(*dock)
        });
        if let Some(dock) = dock {
            unsafe { synchronize(dock.editor, dock.generation)?; }
        }
        Ok(())
    }

    unsafe extern "system" fn proc(
        hwnd: HWND,
        message: u32,
        wp: WPARAM,
        lp: LPARAM,
        id: usize,
        generation: usize,
    ) -> LRESULT {
        if message == RESTORE {
            let restore = RESTORES.with(|restores| {
                let mut restores = restores.borrow_mut();
                if restores
                    .get(&(hwnd.0 as isize))
                    .is_some_and(|restore| restore.generation == generation && wp.0 == generation)
                {
                    restores.remove(&(hwnd.0 as isize))
                } else {
                    None
                }
            });
            if let Some(restore) = restore {
                let _ = RemoveWindowSubclass(hwnd, Some(proc), id);
                if owned_root(hwnd) && frame(hwnd).is_ok_and(|(outer, _)| outer == restore.expected)
                {
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        restore.original.left,
                        restore.original.top,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                    );
                }
            }
            return LRESULT(0);
        }
        if message == WM_NCDESTROY {
            forget_frame(hwnd);
            RESTORES.with(|restores| restores.borrow_mut().remove(&(hwnd.0 as isize)));
        }
        let Some(dock) = relation(hwnd, generation) else {
            if message == WM_NCDESTROY {
                let _ = RemoveWindowSubclass(hwnd, Some(proc), id);
            }
            return DefSubclassProc(hwnd, message, wp, lp);
        };
        if message == WM_NCDESTROY {
            remove(dock.editor, generation, hwnd.0 as isize == dock.editor);
            let _ = RemoveWindowSubclass(hwnd, Some(proc), id);
            return DefSubclassProc(hwnd, message, wp, lp);
        }
        let is_editor = hwnd.0 as isize == dock.editor;
        if !is_editor && message == WM_ENTERSIZEMOVE {
            DOCKS.with(|docks| {
                if let Some(dock) = docks
                    .borrow_mut()
                    .get_mut(&dock.editor)
                    .filter(|dock| dock.generation == generation)
                {
                    dock.owner_untouched = false;
                    dock.restore_owner = None;
                }
            });
        }
        if message == SYNC && is_editor {
            if wp.0 == generation {
                if let Err(error) = synchronize(dock.editor, generation) {
                    eprintln!("ClipHost editor docking: {error}");
                }
            }
            return LRESULT(0);
        }
        if is_editor && message == WM_WINDOWPOSCHANGING && lp.0 != 0 && !dock.syncing {
            let pos = &mut *(lp.0 as *mut WINDOWPOS);
            if let (Ok((_, owner)), Ok((outer, visible))) =
                (frame(HWND(dock.owner as *mut _)), frame(hwnd))
            {
                if !pos.flags.contains(SWP_NOSIZE) {
                    let width = (pos.cx - (outer.width - visible.width)).max(1);
                    DOCKS.with(|docks| {
                        if let Some(dock) = docks
                            .borrow_mut()
                            .get_mut(&dock.editor)
                            .filter(|dock| dock.generation == generation)
                        {
                            dock.preferred_width_dip =
                                f64::from(width) * 96.0 / f64::from(GetDpiForWindow(hwnd).max(96));
                        }
                    });
                } else {
                    pos.cx = outer.width;
                }
                pos.x = owner.left + owner.width - (visible.left - outer.left);
                pos.y = owner.top - (visible.top - outer.top);
                pos.cy = owner.height + outer.height - visible.height;
                pos.flags &= !(SWP_NOMOVE | SWP_NOSIZE);
            }
        }
        if is_editor && message == WM_SYSCOMMAND {
            match wp.0 as u32 & 0xfff0 {
                SC_MOVE | SC_MAXIMIZE | SC_MINIMIZE => return LRESULT(0),
                _ => {}
            }
        }
        if is_editor && message == WM_NCHITTEST {
            let hit = DefSubclassProc(hwnd, message, wp, lp);
            return LRESULT(match hit.0 as u32 {
                HTTOPRIGHT | HTBOTTOMRIGHT => HTRIGHT,
                HTCAPTION | HTLEFT | HTTOP | HTBOTTOM | HTTOPLEFT | HTBOTTOMLEFT => HTCLIENT,
                _ => return hit,
            } as isize);
        }
        if is_editor && message == WM_NCLBUTTONDOWN && wp.0 as u32 == HTRIGHT {
            return DefWindowProcW(hwnd, message, wp, lp);
        }
        if is_editor && message == WM_GETMINMAXINFO && lp.0 != 0 {
            let result = DefSubclassProc(hwnd, message, wp, lp);
            if let (Ok((_, owner)), Ok((outer, visible)), Ok(work)) = (
                frame(HWND(dock.owner as *mut _)),
                frame(hwnd),
                work_area(HWND(dock.owner as *mut _)),
            ) {
                let info = &mut *(lp.0 as *mut MINMAXINFO);
                let height = owner.height + outer.height - visible.height;
                info.ptMinTrackSize.y = height;
                info.ptMaxTrackSize.y = height;
                let minimum = minimum_width(HWND(dock.owner as *mut _), work, dock.minimum_width_dip);
                let width = (work.width - owner.width).max(minimum) + outer.width - visible.width;
                info.ptMaxTrackSize.x = width;
                info.ptMinTrackSize.x = minimum + outer.width - visible.width;
            }
            return result;
        }
        let result = DefSubclassProc(hwnd, message, wp, lp);
        if !is_editor && message == WM_WINDOWPOSCHANGED {
            if let Ok((outer, _)) = frame(hwnd) {
                DOCKS.with(|docks| {
                    if let Some(dock) = docks
                        .borrow_mut()
                        .get_mut(&dock.editor)
                        .filter(|dock| dock.generation == generation && !dock.syncing)
                    {
                        if outer != dock.last_owner {
                            dock.owner_untouched = false;
                            dock.restore_owner = None;
                            dock.last_owner = outer;
                        }
                    }
                });
            }
        }
        if matches!(
            message,
            WM_WINDOWPOSCHANGED | WM_SHOWWINDOW | WM_DPICHANGED | WM_EXITSIZEMOVE
        ) {
            queue(dock);
        }
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::{core::w, Win32::System::LibraryLoader::GetModuleHandleW};

        struct Fixture(HWND);
        impl Fixture {
            unsafe fn new(rect: BoxRect) -> Self {
                Self(
                    CreateWindowExW(
                        WS_EX_NOACTIVATE,
                        w!("STATIC"),
                        w!("ClipHost editor docking hidden fixture"),
                        WS_POPUP,
                        rect.left,
                        rect.top,
                        rect.width,
                        rect.height,
                        None,
                        None,
                        Some(GetModuleHandleW(None).unwrap().into()),
                        None,
                    )
                    .unwrap(),
                )
            }
            unsafe fn rect(&self) -> BoxRect {
                frame(self.0).unwrap().0
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.0);
                }
            }
        }
        unsafe fn flush(editor: HWND) {
            let mut message = MSG::default();
            for _ in 0..16 {
                if !PeekMessageW(&mut message, Some(editor), SYNC, RESTORE, PM_REMOVE).as_bool() {
                    break;
                }
                let _ = DispatchMessageW(&message);
            }
        }
        fn rect(left: i32, top: i32, width: i32, height: i32) -> BoxRect {
            BoxRect {
                left,
                top,
                width,
                height,
            }
        }

        #[test]
        fn companion_frame_insets_follow_current_geometry_and_dpi() {
            let insets = FrameInsets::measured(
                rect(20, 30, 516, 708), rect(28, 30, 500, 700), 96,
            );
            assert_eq!(insets.visible(rect(700, 80, 516, 708), 96), rect(708, 80, 500, 700));
            assert_eq!(insets.visible(rect(-900, 50, 774, 1062), 144), rect(-888, 50, 750, 1050));
            assert_eq!(insets.visible(rect(50, 60, 645, 885), 120), rect(60, 60, 625, 875));
            assert_eq!(insets.visible(rect(20, 30, 516, 708), 96), rect(28, 30, 500, 700));
        }

        #[test]
        fn lan_companion_repeated_reopen_preserves_width_with_resize_margins() {
            unsafe {
                let owner = Fixture::new(rect(100, 120, 260, 420));
                let lan = Fixture::new(rect(20, 30, 516, 708));
                // The normal product dispatcher outlives individual docking
                // sessions and must clean up even when destroyed undocked.
                assert!(SetWindowSubclass(lan.0, Some(crate::platform::host_style_proc),
                    crate::platform::HOST_STYLE_SUBCLASS, 2).as_bool());
                // Hidden popup fixtures have no DWM frame. Supply the insets
                // already measured for a shown window, including nonzero
                // invisible resize margins, without showing UI on the desktop.
                let insets = FrameInsets::measured(
                    lan.rect(), rect(28, 30, 500, 700), GetDpiForWindow(lan.0).max(96),
                );
                FRAME_INSETS.with(|frames| frames.borrow_mut().insert(lan.0 .0 as isize, insets));
                register_with_minimum(lan.0, owner.0 .0 as isize, 320.).unwrap();
                let original_width = lan.rect().width;
                let original_dip = DOCKS.with(|docks| docks.borrow()[&(lan.0 .0 as isize)].preferred_width_dip);
                assert_eq!(original_width, 516);
                let editor = Fixture::new(rect(20, 30, 480, 250));
                for step in 0..24 {
                    unregister(lan.0).unwrap();
                    let _ = ShowWindow(lan.0, SW_HIDE);
                    if step % 2 == 0 {
                        register(editor.0, owner.0 .0 as isize).unwrap();
                        unregister(editor.0).unwrap();
                    }
                    SetWindowPos(owner.0, None, 100 + step, 120 + step, 0, 0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE).unwrap();
                    register_with_minimum(lan.0, owner.0 .0 as isize, 320.).unwrap();
                    flush(lan.0);
                    let (outer, visible) = frame(lan.0).unwrap();
                    assert_eq!(outer.width, original_width, "reopen {step}");
                    assert_eq!(visible.width, 500, "reopen {step}");
                    assert_eq!(visible.left, owner.rect().left + owner.rect().width);
                    assert_eq!((visible.top, visible.height), (owner.rect().top, owner.rect().height));
                    assert_eq!(DOCKS.with(|docks| docks.borrow()[&(lan.0 .0 as isize)].preferred_width_dip), original_dip);
                    set_enabled(lan.0, false).unwrap();
                    set_enabled(lan.0, true).unwrap();
                    assert_eq!(lan.rect().width, original_width, "suspend {step}");
                }
                // A real horizontal resize becomes the retained width, rather
                // than resetting to the default whenever the pane is opened.
                SetWindowPos(lan.0, None, 0, 0, 616, 80,
                    SWP_NOZORDER | SWP_NOACTIVATE).unwrap();
                flush(lan.0);
                assert_eq!(frame(lan.0).unwrap().1.width, 600);
                for _ in 0..24 {
                    unregister(lan.0).unwrap();
                    register_with_minimum(lan.0, owner.0 .0 as isize, 320.).unwrap();
                    flush(lan.0);
                    assert_eq!(lan.rect().width, 616);
                    assert_eq!(frame(lan.0).unwrap().1.width, 600);
                }
                unregister(lan.0).unwrap();
                DestroyWindow(lan.0).unwrap();
                assert!(!FRAME_INSETS.with(|frames| frames.borrow().contains_key(&(lan.0 .0 as isize))));
                assert!(!IsWindowVisible(owner.0).as_bool());
            }
        }

        #[test]
        fn editor_docking_placement_keeps_the_pair_inside_the_work_area() {
            assert_eq!(
                placement(rect(1600, 40, 420, 700), 640, 420, rect(0, 0, 1920, 1080), true),
                (860, rect(1280, 40, 640, 700))
            );
            assert_eq!(
                placement(rect(-400, 40, 420, 700), 800, 400, rect(-800, 0, 800, 900), true),
                (-820, rect(-400, 40, 400, 700))
            );
            assert_eq!(
                placement(rect(0, 40, 800, 700), 600, 400, rect(0, 0, 800, 900), true),
                (-400, rect(400, 40, 400, 700))
            );
        }

        #[test]
        fn folded_companion_reenters_topmost_without_lowering_its_visible_owner() {
            unsafe {
                // Keep both HWNDs off screen even if docking adjusts their X.
                let owner = Fixture::new(rect(-32000, -32000, 260, 420));
                let editor = Fixture::new(rect(-32000, -32000, 300, 250));
                let topmost = |hwnd| GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize != 0;
                super::super::super::raw_show(owner.0, true, false).unwrap();
                assert!(IsWindowVisible(owner.0).as_bool(), "owner must be visible before register");
                assert!(topmost(owner.0), "owner must be topmost before register");
                register(editor.0, owner.0 .0 as isize).unwrap();
                flush(editor.0);
                assert!(IsWindowVisible(owner.0).as_bool(), "owner became hidden while registering/flushing companion");
                assert!(topmost(owner.0), "owner lost topmost while registering/flushing companion");
                assert!(IsWindowVisible(editor.0).as_bool());
                assert!(topmost(editor.0), "a newly opened companion must share its owner's layer");

                set_enabled(editor.0, false).unwrap();
                flush(editor.0);
                assert!(IsWindowVisible(owner.0).as_bool() && topmost(owner.0));
                assert!(!IsWindowVisible(editor.0).as_bool());
                assert!(!topmost(editor.0), "a folded companion must leave the topmost band");

                set_enabled(editor.0, true).unwrap();
                flush(editor.0);
                assert!(IsWindowVisible(owner.0).as_bool() && topmost(owner.0));
                assert!(IsWindowVisible(editor.0).as_bool());
                assert!(topmost(editor.0), "unfold must restore the visible companion's topmost layer");
            }
        }

        #[test]
        fn editor_docking_hidden_hwnds_follow_owner_and_keep_horizontal_resize() {
            unsafe {
                let owner = Fixture::new(rect(100, 120, 260, 420));
                let editor = Fixture::new(rect(20, 30, 300, 250));
                register(editor.0, owner.0 .0 as isize).unwrap();
                let first = editor.rect();
                let first_owner = owner.rect();
                assert_eq!(first.left, first_owner.left + first_owner.width);
                assert_eq!(
                    (first.top, first.height),
                    (first_owner.top, first_owner.height)
                );
                assert_eq!(
                    GetWindowLongPtrW(editor.0, GWLP_HWNDPARENT),
                    owner.0 .0 as isize
                );
                assert_ne!(
                    GetWindowLongPtrW(editor.0, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as isize,
                    0
                );
                SetWindowPos(
                    owner.0,
                    None,
                    180,
                    160,
                    310,
                    510,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
                .unwrap();
                // The owner's synchronous position callback only queues work.
                assert_eq!(editor.rect(), first);
                flush(editor.0);
                let second = editor.rect();
                let second_owner = owner.rect();
                assert_eq!(second.left, second_owner.left + second_owner.width);
                assert_eq!((second.top, second.height), (160, 510));
                SetWindowPos(editor.0, None, 0, 0, 590, 80, SWP_NOZORDER | SWP_NOACTIVATE).unwrap();
                flush(editor.0);
                let work = work_area(owner.0).unwrap();
                let (_, expected) =
                    placement(owner.rect(), 590, minimum_width(owner.0, work, f64::from(crate::editor::minimum_width_dip())), work, false);
                assert_eq!(editor.rect(), expected);
                let dock =
                    DOCKS.with(|docks| *docks.borrow().get(&(editor.0 .0 as isize)).unwrap());
                let preferred = dock.preferred_width_dip;
                register(editor.0, owner.0 .0 as isize).unwrap();
                assert_eq!(
                    current(dock.editor, dock.generation)
                        .unwrap()
                        .preferred_width_dip,
                    preferred
                );
                assert!(!IsWindowVisible(owner.0).as_bool());
                assert!(!IsWindowVisible(editor.0).as_bool());
            }
        }

        #[test]
        fn editor_docking_destroy_removes_both_subclasses_and_stale_messages() {
            unsafe {
                let owner = Fixture::new(rect(100, 120, 260, 420));
                let editor = Fixture::new(rect(20, 30, 300, 250));
                register(editor.0, owner.0 .0 as isize).unwrap();
                let dock =
                    DOCKS.with(|docks| *docks.borrow().get(&(editor.0 .0 as isize)).unwrap());
                DestroyWindow(editor.0).unwrap();
                assert!(current(dock.editor, dock.generation).is_none());
                let mut data = 0;
                assert!(!windows::Win32::UI::Shell::GetWindowSubclass(
                    owner.0,
                    Some(proc),
                    SUBCLASS,
                    Some(&mut data)
                )
                .as_bool());
                SetWindowPos(
                    owner.0,
                    None,
                    150,
                    160,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                )
                .unwrap();
                // A stale queued generation cannot create a replacement relation.
                synchronize(dock.editor, dock.generation).unwrap();
                assert!(current(dock.editor, dock.generation).is_none());
            }
        }

        #[test]
        fn editor_docking_close_restores_only_the_automatic_owner_shift() {
            unsafe {
                let owner = Fixture::new(rect(100, 120, 260, 420));
                let work = work_area(owner.0).unwrap();
                let original = rect(work.left + work.width - 260, work.top + 40, 260, 420);
                SetWindowPos(
                    owner.0,
                    None,
                    original.left,
                    original.top,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                )
                .unwrap();
                let editor = Fixture::new(rect(20, 30, 500, 250));
                register(editor.0, owner.0 .0 as isize).unwrap();
                let shifted = owner.rect();
                assert!(shifted.left < original.left);
                DestroyWindow(editor.0).unwrap();
                assert_eq!(owner.rect(), shifted, "destroy only posts restoration");
                flush(owner.0);
                assert_eq!(owner.rect(), original);
                assert!(RESTORES.with(|restores| restores.borrow().is_empty()));

                let editor = Fixture::new(rect(20, 30, 500, 250));
                register(editor.0, owner.0 .0 as isize).unwrap();
                let shifted = owner.rect();
                SetWindowPos(
                    owner.0,
                    None,
                    shifted.left - 12,
                    shifted.top + 12,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                )
                .unwrap();
                flush(editor.0);
                let user_moved = owner.rect();
                DestroyWindow(editor.0).unwrap();
                flush(owner.0);
                assert_eq!(
                    owner.rect(),
                    user_moved,
                    "a later owner change cancels restoration"
                );
            }
        }

        #[test]
        fn lan_companion_edge_drag_never_repositions_the_owner_after_move_begins() {
            unsafe {
                let owner = Fixture::new(rect(100, 120, 260, 420));
                let work = work_area(owner.0).unwrap();
                let initial = rect(work.left + work.width - 260, work.top + 40, 260, 420);
                SetWindowPos(owner.0, None, initial.left, initial.top, 0, 0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE).unwrap();
                let lan = Fixture::new(rect(20, 30, 400, 250));
                register_with_minimum(lan.0, owner.0 .0 as isize, 320.).unwrap();
                assert!(owner.rect().left < initial.left, "first opening still makes room for the pair");
                let dock = DOCKS.with(|docks| *docks.borrow().get(&(lan.0 .0 as isize)).unwrap());

                // Deliver only the owned HWND lifecycle messages; do not enter
                // a desktop SC_MOVE loop, move the system cursor or show UI.
                SendMessageW(owner.0, WM_ENTERSIZEMOVE, Some(WPARAM(0)), Some(LPARAM(0)));
                let mut final_owner = initial;
                for (step, inset) in [0, 6, 12, 53].into_iter().enumerate() {
                    let requested = rect(initial.left - inset, initial.top + step as i32 * 11, 260, 420);
                    SetWindowPos(owner.0, None, requested.left, requested.top, 0, 0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE).unwrap();
                    assert_eq!(owner.rect(), requested);
                    for _ in 0..3 {
                        flush(lan.0);
                        assert_eq!(owner.rect(), requested, "posted synchronization cannot fight the cursor's owner position");
                        let companion = lan.rect();
                        assert_eq!(companion.left, requested.left + requested.width);
                        assert_eq!((companion.top, companion.height), (requested.top, requested.height));
                    }
                    final_owner = requested;
                }
                SendMessageW(owner.0, WM_EXITSIZEMOVE, Some(WPARAM(0)), Some(LPARAM(0)));
                flush(lan.0);
                assert_eq!(owner.rect(), final_owner, "release cannot apply a deferred work-area pullback");
                register_with_minimum(lan.0, owner.0 .0 as isize, 320.).unwrap();
                flush(lan.0);
                assert_eq!(owner.rect(), final_owner, "re-presenting this same pane preserves the user's position");
                assert_eq!(lan.rect().left, final_owner.left + final_owner.width);
                DestroyWindow(lan.0).unwrap();
                flush(owner.0);
                assert_eq!(owner.rect(), final_owner, "closing cannot restore the pre-drag automatic-shift position");
                assert!(current(dock.editor, dock.generation).is_none());
                assert!(!IsWindowVisible(owner.0).as_bool());
            }
        }

        #[test]
        fn lan_companion_external_owner_moves_are_authoritative_without_a_move_loop() {
            unsafe {
                let owner = Fixture::new(rect(100, 120, 260, 420));
                let work = work_area(owner.0).unwrap();
                let initial = rect(work.left + work.width - 260, work.top + 40, 260, 420);
                SetWindowPos(owner.0, None, initial.left, initial.top, 0, 0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE).unwrap();
                let lan = Fixture::new(rect(20, 30, 400, 250));
                register_with_minimum(lan.0, owner.0 .0 as isize, 320.).unwrap();
                assert!(owner.rect().left < initial.left);
                // An outside geometry writer need not produce ENTER/EXIT.
                // Its repeated position changes must not become a second
                // feedback loop with the companion's queued placement.
                let mut final_owner = initial;
                for inset in [8, 2, 31] {
                    let requested = rect(initial.left - inset, initial.top + 17, 260, 420);
                    SetWindowPos(owner.0, None, requested.left, requested.top, 0, 0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE).unwrap();
                    flush(lan.0);
                    assert_eq!(owner.rect(), requested);
                    assert_eq!(lan.rect().left, requested.left + requested.width);
                    final_owner = requested;
                }
                register_with_minimum(lan.0, owner.0 .0 as isize, 320.).unwrap();
                flush(lan.0);
                assert_eq!(owner.rect(), final_owner);
                DestroyWindow(lan.0).unwrap();
                flush(owner.0);
                assert_eq!(owner.rect(), final_owner);
                assert!(!IsWindowVisible(owner.0).as_bool());
            }
        }

        #[test]
        fn editor_docking_rejects_other_threads_and_self_ownership() {
            unsafe {
                let owner = Fixture::new(rect(100, 120, 260, 420));
                assert!(register(owner.0, owner.0 .0 as isize).is_err());
                let owner_bits = owner.0 .0 as isize;
                assert!(std::thread::spawn(move || {
                    let editor = Fixture::new(rect(20, 30, 300, 250));
                    register(editor.0, owner_bits).is_err()
                })
                .join()
                .unwrap());
                assert!(DOCKS.with(|docks| docks.borrow().is_empty()));
            }
        }

        #[test]
        fn lan_companion_suspend_and_reopen_keep_only_the_current_slot() {
            unsafe {
                let owner = Fixture::new(rect(100, 120, 260, 420));
                let lan = Fixture::new(rect(20, 30, 400, 250));
                register_with_minimum(lan.0, owner.0 .0 as isize, 320.).unwrap();
                let first = DOCKS.with(|docks| *docks.borrow().get(&(lan.0 .0 as isize)).unwrap());
                assert_eq!(first.minimum_width_dip, 320.);
                set_enabled(lan.0, false).unwrap();
                assert!(!current(first.editor, first.generation).unwrap().enabled);
                assert!(!IsWindowVisible(lan.0).as_bool());
                set_enabled(lan.0, true).unwrap();
                assert!(current(first.editor, first.generation).unwrap().enabled);

                unregister(lan.0).unwrap();
                assert!(current(first.editor, first.generation).is_none());
                // Owner movement and stale posted work cannot revive a closed
                // pane, even though its HWND and page entity are reusable.
                synchronize(first.editor, first.generation).unwrap();
                let editor = Fixture::new(rect(20, 30, 480, 250));
                register(editor.0, owner.0 .0 as isize).unwrap();
                assert_eq!(DOCKS.with(|docks| docks.borrow().len()), 1);
                unregister(editor.0).unwrap();
                register_with_minimum(lan.0, owner.0 .0 as isize, 320.).unwrap();
                let next = DOCKS.with(|docks| *docks.borrow().get(&(lan.0 .0 as isize)).unwrap());
                assert_ne!(next.generation, first.generation);
                assert_eq!(DOCKS.with(|docks| docks.borrow().len()), 1);
                assert!(!IsWindowVisible(lan.0).as_bool(), "hidden owners keep the reopened companion hidden");
            }
        }
    }
}

#[cfg(windows)]
pub(crate) use win::{forget_frame, register, register_with_minimum, set_enabled, unregister};
