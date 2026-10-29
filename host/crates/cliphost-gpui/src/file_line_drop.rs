//! The file line's OLE boundary. The hidden line observes real drag start/end
//! events, never clipboard contents. Only CF_HDROP reaches the line's queue;
//! directory checks and settings writes belong on its background worker.

use crate::drag_drop::{DropError, DropEvent, DropTicket};
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

pub(super) enum Event {
    Dropped(DropEvent),
    Error(DropError),
    Sent { ticket: DropTicket, result: Result<serde_json::Value, String> },
}

#[derive(Default)]
pub(super) struct FileLineDrop {
    events: Rc<RefCell<VecDeque<Event>>>,
    hover: Rc<RefCell<Option<DropTicket>>>,
    #[cfg(windows)]
    binding: Option<crate::drag_drop::DragDropWindow>,
    #[cfg(windows)]
    outgoing: Option<crate::drag_drop::DragOutDispatcher>,
    #[cfg(windows)]
    sent: Option<(DropTicket, std::sync::mpsc::Receiver<Result<serde_json::Value, String>>)>,
    #[cfg(windows)]
    monitor: Option<DragMonitor>,
    hwnd: isize,
}

impl FileLineDrop {
    /// Sync from GPUI after its own HWND exists. COM observers only enqueue;
    /// borrowing App in a provider's nested message loop is forbidden.
    pub fn sync(&mut self, hwnd: isize, session: u64, accepting: bool, receiving: bool) -> Result<(), String> {
        #[cfg(windows)]
        {
            if hwnd == 0 { self.cancel(); return Ok(()); }
            if self.hwnd != hwnd || self.binding.is_none() {
                self.cancel();
                self.binding = None;
                self.outgoing = None;
                self.hwnd = hwnd;
                let events = self.events.clone();
                let errors = self.events.clone();
                let hover = self.hover.clone();
                self.binding = Some(crate::drag_drop::DragDropWindow::attach_files(
                    hwnd,
                    Rc::new(move |event| {
                        event.ticket.verify()?;
                        if event.formats.files.is_empty() { return Err("没有可挂上的文件夹路径。".into()); }
                        // Bounded even when the UI thread is busy with another
                        // native dispatch. Rejection never advertises MOVE.
                        let mut queue = events.borrow_mut();
                        if queue.len() >= 16 { return Err("请等待上次拖放完成。".into()); }
                        queue.push_back(Event::Dropped(event));
                        Ok(())
                    }),
                    Rc::new(move |event| match event {
                        crate::drag_drop::DragOverEvent::Enter(ticket) => *hover.borrow_mut() = Some(ticket),
                        crate::drag_drop::DragOverEvent::Leave(ticket) => {
                            if hover.borrow().as_ref().is_some_and(|current| current.same_request(&ticket)) {
                                *hover.borrow_mut() = None;
                            }
                        }
                    }),
                    Rc::new(move |error| {
                        let mut queue = errors.borrow_mut();
                        if queue.len() < 16 { queue.push_back(Event::Error(error)); }
                    }),
                )?);
                self.outgoing = Some(crate::drag_drop::DragOutDispatcher::attach(
                    hwnd,
                    std::sync::Arc::new(|payload, ticket| cliphost_core::ole_drag::begin_ole_drag_guarded(
                        payload, std::sync::Arc::new(move || ticket.verify()),
                    )),
                )?);
            }
            if let Some(binding) = &self.binding { binding.sync_session(session, accepting && receiving); }
            if let Some(outgoing) = &self.outgoing { outgoing.sync_session(session, accepting); }
        }
        #[cfg(not(windows))]
        let _ = (hwnd, session, accepting, receiving);
        Ok(())
    }

    pub fn cancel(&mut self) {
        #[cfg(windows)]
        {
            if let Some(binding) = &self.binding { binding.cancel(); }
            if let Some(outgoing) = &self.outgoing { outgoing.cancel(); }
            self.sent = None;
            self.monitor = None;
        }
        *self.hover.borrow_mut() = None;
        self.events.borrow_mut().clear();
    }

    pub fn hovering(&self) -> bool {
        self.hover.borrow().as_ref().is_some_and(DropTicket::is_current)
    }

    /// A held mouse button alone is not a drag. Sources which do not publish
    /// accessibility drag events still use the normal two-stage edge reveal.
    pub fn active_drag(&mut self) -> bool {
        #[cfg(windows)]
        {
            if self.monitor.is_none() { self.monitor = DragMonitor::install(); }
            self.monitor.as_ref().is_some_and(DragMonitor::active)
        }
        #[cfg(not(windows))]
        { false }
    }

    pub fn outgoing_active(&self) -> bool {
        #[cfg(windows)]
        { self.sent.is_some() }
        #[cfg(not(windows))]
        { false }
    }

    /// Call as soon as a native drag gesture crosses its threshold, while the
    /// button is still down. No clipboard write and no Shell-open fallback.
    pub fn start_files(&mut self, host: &cliphost_core::host::HostState, paths: Vec<String>) -> Result<(), String> {
        if paths.is_empty() || paths.iter().any(|path| path.is_empty() || path.contains('\0')) {
            return Err("没有可拖出的文件路径。".into());
        }
        #[cfg(windows)]
        {
            if self.sent.is_some() { return Err("已有文件正在拖动。".into()); }
            let outgoing = self.outgoing.as_ref().ok_or("文件拖动窗口尚未就绪。")?;
            let host = outgoing.bound_host(host);
            self.sent = Some(outgoing.start_ticketed(move || host.file_line_drag_files(paths))?);
            Ok(())
        }
        #[cfg(not(windows))]
        { let _ = (host, paths); Err("文件拖出仅支持 Windows。".into()) }
    }

    pub fn drain(&mut self) -> Vec<Event> {
        #[cfg(windows)]
        if let Some((ticket, receiver)) = &self.sent {
            let result = match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err("文件拖动线程已结束。".into())),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                let ticket = ticket.clone();
                self.sent = None;
                self.events.borrow_mut().push_back(Event::Sent { ticket, result });
            }
        }
        self.events.borrow_mut().drain(..).filter(|event| match event {
            Event::Dropped(event) => event.ticket.is_current(),
            Event::Error(error) => error.ticket.is_current(),
            Event::Sent { ticket, .. } => ticket.is_current(),
        }).collect()
    }
}

/// Inspect metadata only, on a worker. The whole offer must be directories;
/// mixed files/folders are rejected without partially changing the menu.
pub(super) fn directories(paths: &[String]) -> Result<Vec<String>, String> {
    if paths.is_empty() || paths.len() > crate::drag_drop::MAX_DROP_FILES {
        return Err("请拖入文件夹。".into());
    }
    let mut result = Vec::new();
    for path in paths {
        if path.contains('\0') || !std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
            return Err("只能把现有文件夹挂上晾衣绳。".into());
        }
        if !result.iter().any(|existing: &String| existing.eq_ignore_ascii_case(path)) {
            result.push(path.clone());
        }
    }
    Ok(result)
}

/// Accessibility notifications are a hint to expose an actual OLE target,
/// not proof of a file payload. DragEnter/Drop still enforce CF_HDROP.
#[cfg(windows)]
struct DragMonitor(windows_compat::Win32::UI::Accessibility::HWINEVENTHOOK);

#[cfg(windows)]
thread_local! { static DRAG_STARTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

#[cfg(windows)]
unsafe extern "system" fn drag_event(
    _: windows_compat::Win32::UI::Accessibility::HWINEVENTHOOK,
    event: u32, _: windows_compat::Win32::Foundation::HWND,
    _: i32, _: i32, _: u32, _: u32,
) {
    use windows_compat::Win32::UI::WindowsAndMessaging::{EVENT_SYSTEM_DRAGDROPEND, EVENT_SYSTEM_DRAGDROPSTART};
    if event == EVENT_SYSTEM_DRAGDROPSTART || event == EVENT_SYSTEM_DRAGDROPEND {
        DRAG_STARTED.with(|started| started.set(event == EVENT_SYSTEM_DRAGDROPSTART));
    }
}

#[cfg(windows)]
impl DragMonitor {
    fn install() -> Option<Self> {
        use windows_compat::Win32::UI::{Accessibility::SetWinEventHook, WindowsAndMessaging::{EVENT_SYSTEM_DRAGDROPEND, EVENT_SYSTEM_DRAGDROPSTART, WINEVENT_OUTOFCONTEXT}};
        let hook = unsafe { SetWinEventHook(EVENT_SYSTEM_DRAGDROPSTART, EVENT_SYSTEM_DRAGDROPEND, windows_compat::Win32::Foundation::HMODULE::default(), Some(drag_event), 0, 0, WINEVENT_OUTOFCONTEXT) };
        (!hook.is_invalid()).then_some(Self(hook))
    }
    fn active(&self) -> bool {
        use windows_compat::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
        let pressed = [VK_LBUTTON, VK_RBUTTON].into_iter().any(|key| unsafe { GetAsyncKeyState(i32::from(key.0)) } as u16 & 0x8000 != 0);
        DRAG_STARTED.with(|started| {
            if !pressed { started.set(false); }
            started.get()
        })
    }
}

#[cfg(windows)]
impl Drop for DragMonitor {
    fn drop(&mut self) {
        let _ = unsafe { windows_compat::Win32::UI::Accessibility::UnhookWinEvent(self.0) };
        DRAG_STARTED.with(|started| started.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_line_drop_accepts_only_directories_without_partial_mixed_drops() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::Builder::new().prefix("file-line-drop-").tempdir_in(root).unwrap();
        let folder = dir.path().join("folder");
        let file = dir.path().join("file.txt");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(&file, "fixture").unwrap();
        let folder = folder.to_str().unwrap().to_string();
        let file = file.to_str().unwrap().to_string();
        assert_eq!(directories(&[folder.clone(), folder.clone()]).unwrap(), vec![folder.clone()]);
        assert!(directories(&[folder, file]).is_err());
        assert!(directories(&[]).is_err());
    }
}
