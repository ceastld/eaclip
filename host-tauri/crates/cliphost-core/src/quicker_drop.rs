//! STA-owned OLE interception for the panel. Ordinary HTML/file drags are forwarded
//! to the original WebView target; a Quicker action is always a copy, never a move.
use std::{cell::Cell, rc::Rc};
use windows::core::{implement, w, Interface, Result, HSTRING};
use windows::Win32::{
    Foundation::{BOOL, E_POINTER, HWND, LPARAM, POINTL},
    System::{
        Com::{
            CoTaskMemFree, IDataObject, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, STGMEDIUM,
            TYMED_HGLOBAL,
        },
        DataExchange::{GetClipboardFormatNameW, RegisterClipboardFormatW},
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
        Ole::{
            IDropTarget, IDropTarget_Impl, OleInitialize, OleUninitialize, RegisterDragDrop,
            ReleaseStgMedium, RevokeDragDrop, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
        },
        SystemServices::MODIFIERKEYS_FLAGS,
        Threading::{GetCurrentProcessId, GetCurrentThreadId},
    },
    UI::WindowsAndMessaging::{EnumChildWindows, GetPropW, GetWindowThreadProcessId},
};

type OnDrop = Rc<dyn Fn(std::result::Result<String, String>) -> std::result::Result<(), String>>;
type OnHover = Rc<dyn Fn(bool)>;
const FORMATS: &[&str] = &[
    "quicker-action-item-json",
    "quicker-action-drag-item",
    "quicker-action-item",
    "Quicker.Common.ActionItemDragObject",
    "Quicker.Common.ActionItem",
];

fn format(id: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: id,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}
fn action_formats(data: &IDataObject) -> Vec<u16> {
    let mut formats: Vec<u16> = FORMATS
        .iter()
        .map(|name| unsafe { RegisterClipboardFormatW(&HSTRING::from(*name)) as u16 })
        .filter(|&id| id != 0)
        .collect();
    if let Ok(enumerator) = unsafe { data.EnumFormatEtc(DATADIR_GET.0 as u32) } {
        for _ in 0..256 {
            let mut entry = [FORMATETC::default()];
            let mut fetched = 0;
            let result = unsafe { enumerator.Next(&mut entry, Some(&mut fetched)) };
            if result.is_err() || fetched == 0 {
                break;
            }
            let id = entry[0].cfFormat;
            if !entry[0].ptd.is_null() {
                unsafe {
                    CoTaskMemFree(Some(entry[0].ptd.cast()));
                }
            }
            let mut name = [0u16; 256];
            let len = unsafe { GetClipboardFormatNameW(id as u32, &mut name) };
            let name = String::from_utf16_lossy(&name[..len.max(0) as usize]).to_ascii_lowercase();
            if (name.contains("quicker-action") || name.contains("actionitemdrag"))
                && !formats.contains(&id)
            {
                formats.push(id);
            }
        }
    }
    formats
}
fn has_action(data: &IDataObject) -> bool {
    action_formats(data)
        .into_iter()
        .any(|id| unsafe { data.QueryGetData(&format(id)).is_ok() })
}
struct Medium(STGMEDIUM);
impl Drop for Medium {
    fn drop(&mut self) {
        unsafe {
            ReleaseStgMedium(&mut self.0);
        }
    }
}
fn action_payload(data: &IDataObject) -> Option<String> {
    action_formats(data).into_iter().find_map(|id| unsafe {
        let medium = Medium(data.GetData(&format(id)).ok()?);
        if medium.0.tymed != TYMED_HGLOBAL.0 as u32 {
            return None;
        }
        let handle = medium.0.u.hGlobal;
        let size = GlobalSize(handle);
        if size == 0 || size > 32 * 1024 * 1024 {
            return None;
        }
        let ptr = GlobalLock(handle);
        if ptr.is_null() {
            return None;
        }
        let result = crate::quicker_wire::decode(std::slice::from_raw_parts(ptr.cast(), size));
        let _ = GlobalUnlock(handle);
        result
    })
}

#[implement(IDropTarget)]
struct Target {
    original: IDropTarget,
    on_drop: OnDrop,
    on_hover: OnHover,
    quicker: Cell<bool>,
    forwarding: Cell<bool>,
}
impl Target {
    fn reset(&self) {
        self.quicker.set(false);
        self.forwarding.set(false);
        (self.on_hover)(false);
    }
    fn copy_effect(&self, effect: *mut DROPEFFECT) -> Result<()> {
        let effect =
            unsafe { effect.as_mut() }.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
        *effect = DROPEFFECT(effect.0 & DROPEFFECT_COPY.0);
        Ok(())
    }
}
#[allow(non_snake_case)]
impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(
        &self,
        data: Option<&IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        if effect.is_null() {
            return Err(E_POINTER.into());
        }
        self.reset();
        if data.is_some_and(has_action) {
            self.quicker.set(true);
            self.copy_effect(effect)?;
            (self.on_hover)(unsafe { *effect } != DROPEFFECT_NONE);
            Ok(())
        } else {
            self.forwarding.set(true);
            unsafe { self.original.DragEnter(data, keys, *pt, effect) }
        }
    }
    fn DragOver(
        &self,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        if effect.is_null() {
            return Err(E_POINTER.into());
        }
        if self.quicker.get() {
            self.copy_effect(effect)
        } else if self.forwarding.get() {
            unsafe { self.original.DragOver(keys, *pt, effect) }
        } else {
            unsafe {
                *effect = DROPEFFECT_NONE;
            }
            Ok(())
        }
    }
    fn DragLeave(&self) -> Result<()> {
        let forward = self.forwarding.get();
        self.reset();
        if forward {
            unsafe { self.original.DragLeave() }
        } else {
            Ok(())
        }
    }
    fn Drop(
        &self,
        data: Option<&IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        if effect.is_null() {
            return Err(E_POINTER.into());
        }
        let quicker = self.quicker.get();
        let forward = self.forwarding.get();
        self.reset();
        if quicker {
            let can_copy = unsafe { (*effect).0 & DROPEFFECT_COPY.0 != 0 };
            unsafe {
                *effect = DROPEFFECT_NONE;
            }
            if can_copy {
                let payload = data
                    .and_then(action_payload)
                    .ok_or_else(|| "Quicker 动作拖放数据无效。".to_string());
                match (self.on_drop)(payload) {
                    Ok(()) => unsafe {
                        *effect = DROPEFFECT_COPY;
                    },
                    Err(error) => tracing::warn!(%error, "Quicker action drop rejected"),
                }
            }
            Ok(())
        } else if forward {
            unsafe { self.original.Drop(data, keys, *pt, effect) }
        } else {
            unsafe {
                *effect = DROPEFFECT_NONE;
            }
            Ok(())
        }
    }
}

struct Binding {
    hwnd: HWND,
    original: IDropTarget,
    target: IDropTarget,
}
impl Drop for Binding {
    fn drop(&mut self) {
        // HWNDs may have been destroyed/reused or WebView may have installed a new target.
        if owned_target_pointer(self.hwnd) == self.target.as_raw() {
            unsafe {
                if RevokeDragDrop(self.hwnd).is_ok() {
                    if let Err(error) = RegisterDragDrop(self.hwnd, &self.original) {
                        tracing::warn!(%error, "Restoring WebView drop target failed");
                    }
                }
            }
        }
    }
}
fn owned_target_pointer(hwnd: HWND) -> *mut std::ffi::c_void {
    unsafe {
        let mut pid = 0;
        let thread = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if thread != GetCurrentThreadId() || pid != GetCurrentProcessId() {
            return std::ptr::null_mut();
        }
        // This OLE-owned property is an in-process pointer. Never inspect another
        // process/thread, and never revoke a target when this lookup is unavailable.
        GetPropW(hwnd, w!("OleDropTargetInterface")).0
    }
}

/// Keep on the creating STA. Missing native target properties leave WebView untouched.
pub struct Bindings {
    entries: Vec<Binding>,
    on_drop: OnDrop,
    on_hover: OnHover,
}
impl Bindings {
    pub fn new(on_drop: OnDrop, on_hover: OnHover) -> Result<Self> {
        unsafe {
            OleInitialize(None)?;
        }
        Ok(Self {
            entries: Vec::new(),
            on_drop,
            on_hover,
        })
    }
    pub fn refresh(&mut self, root: isize) -> usize {
        self.entries
            .retain(|entry| owned_target_pointer(entry.hwnd) == entry.target.as_raw());
        let mut windows: Vec<HWND> = Vec::new();
        unsafe extern "system" fn collect(hwnd: HWND, param: LPARAM) -> BOOL {
            (*(param.0 as *mut Vec<HWND>)).push(hwnd);
            BOOL(1)
        }
        unsafe {
            let _ = EnumChildWindows(
                HWND(root as _),
                Some(collect),
                LPARAM(&mut windows as *mut _ as isize),
            );
        }
        for hwnd in windows {
            if self.entries.iter().any(|entry| entry.hwnd == hwnd) {
                continue;
            }
            let raw = owned_target_pointer(hwnd);
            if raw.is_null() {
                continue;
            }
            // Retain the original before RevokeDragDrop releases OLE's reference.
            let Some(original) = (unsafe { IDropTarget::from_raw_borrowed(&raw) }).cloned() else {
                continue;
            };
            let target: IDropTarget = Target {
                original: original.clone(),
                on_drop: self.on_drop.clone(),
                on_hover: self.on_hover.clone(),
                quicker: Cell::new(false),
                forwarding: Cell::new(false),
            }
            .into();
            unsafe {
                if RevokeDragDrop(hwnd).is_err() {
                    continue;
                }
                if let Err(error) = RegisterDragDrop(hwnd, &target) {
                    tracing::warn!(%error, "Registering Quicker drop target failed");
                    let _ = RegisterDragDrop(hwnd, &original);
                    continue;
                }
            }
            tracing::info!(hwnd = hwnd.0 as isize, "Panel Quicker drop target attached");
            self.entries.push(Binding {
                hwnd,
                original,
                target,
            });
        }
        self.entries.len()
    }
}
impl Drop for Bindings {
    fn drop(&mut self) {
        self.entries.clear();
        unsafe {
            OleUninitialize();
        }
    }
}

#[cfg(test)]
#[path = "quicker_drop_tests.rs"]
mod tests;
