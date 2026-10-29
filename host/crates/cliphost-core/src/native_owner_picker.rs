//! File selection for independently owned native windows. Main-window picker
//! epochs remain separate; the caller supplies a one-way editor/request lease.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OwnerSnapshot {
    root: isize,
    process_id: u32,
    thread_id: u32,
}

fn retired() -> String {
    crate::file_dialog_pick::NATIVE_PICKER_RETIRED.to_string()
}

fn owner_guard(
    owner: isize,
    current_process: u32,
    still_current: impl Fn() -> bool,
    snapshot: impl Fn(isize) -> Option<OwnerSnapshot>,
) -> Result<impl Fn() -> Result<(), String>, String> {
    if !still_current() {
        return Err(retired());
    }
    let captured = snapshot(owner)
        .filter(|state| {
            owner != 0
                && state.root == owner
                && state.process_id == current_process
                && state.thread_id != 0
        })
        .ok_or_else(retired)?;
    // The window lookup is outside any HostInner lock. It may coincide with
    // accepted close/replacement, so validate the caller's lease afterwards.
    if !still_current() {
        return Err(retired());
    }
    Ok(move || {
        if !still_current() {
            return Err(retired());
        }
        let current = snapshot(owner);
        if current != Some(captured) || !still_current() {
            return Err(retired());
        }
        Ok(())
    })
}

pub(crate) fn pick_files_guarded(
    initial: Option<&str>,
    owner: isize,
    still_current: impl Fn() -> bool,
) -> Result<Vec<String>, String> {
    run_with_owner(owner, std::process::id(), still_current, snapshot_owner, |verify| {
        crate::file_dialog_pick::pick_files_guarded(initial, owner as i64, &|| verify())
    })
}

pub(crate) fn pick_directories_guarded(
    initial: Option<&str>,
    owner: isize,
    still_current: impl Fn() -> bool,
) -> Result<Vec<String>, String> {
    run_with_owner(owner, std::process::id(), still_current, snapshot_owner, |verify| {
        crate::file_dialog_pick::pick_directories_guarded(initial, owner as i64, &|| verify())
    })
}

fn run_with_owner<T>(
    owner: isize,
    current_process: u32,
    still_current: impl Fn() -> bool,
    snapshot: impl Fn(isize) -> Option<OwnerSnapshot>,
    pick: impl FnOnce(&dyn Fn() -> Result<(), String>) -> Result<T, String>,
) -> Result<T, String> {
    let verify = owner_guard(owner, current_process, still_current, snapshot)?;
    verify()?;
    // Keep the original owner throughout prepare/Show/result collection. The
    // shared picker invokes this same guard around its external operations.
    let result = pick(&verify);
    verify()?;
    result
}

#[cfg(all(windows, not(test)))]
fn snapshot_owner(owner: isize) -> Option<OwnerSnapshot> {
    use windows::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{GetAncestor, GetWindowThreadProcessId, IsWindow, GA_ROOT},
    };
    let hwnd = HWND(owner as *mut _);
    unsafe {
        if owner == 0 || !IsWindow(hwnd).as_bool() {
            return None;
        }
        let root = GetAncestor(hwnd, GA_ROOT).0 as isize;
        let mut process_id = 0;
        let thread_id = GetWindowThreadProcessId(hwnd, Some(&mut process_id));
        if !IsWindow(hwnd).as_bool() {
            return None;
        }
        Some(OwnerSnapshot {
            root,
            process_id,
            thread_id,
        })
    }
}

// Unit tests supply explicit snapshots to owner_guard. An unconfigured test
// or an unsupported platform cannot probe a live HWND or open a native modal.
#[cfg(any(test, not(windows)))]
fn snapshot_owner(_: isize) -> Option<OwnerSnapshot> {
    None
}

#[cfg(test)]
#[path = "native_owner_picker_tests.rs"]
mod tests;
