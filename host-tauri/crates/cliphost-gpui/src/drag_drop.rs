//! OLE boundary for native views. Keep COM on the HWND STA and payload work on
//! a worker. The existing core owns output formats, configuration and storage.
//!
//! Incoming observers must validate and queue the immutable DropEvent with its
//! captured destination. Accepted drops keep that destination after navigation.
//! UI replies must recheck the ticket. They must not borrow GPUI App
//! from a COM callback: OLE can pump messages while GetData/DoDragDrop runs.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use cliphost_core::clipboard::ClipboardSnapshot;

// Reuse the production managed-string reader, including its bounded NRBF
// handling. No assembly loading, stringification or rich-text reconstruction.
#[path = "../../cliphost-core/src/clipboard_text.rs"]
mod registered_text;

#[cfg(windows)]
#[path = "drag_drop_windows.rs"]
mod win;
#[cfg(windows)]
pub use win::{DragDropWindow, DragOutDispatcher, NativeDragRunner};

pub const MAX_FORMAT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_TRANSFER_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_DROP_FILES: usize = 4096;

/// Opaque lifecycle stamp, safe to carry to a background worker. A reopened
/// window or directory can never make an earlier stamp current again.
#[derive(Clone, Debug)]
pub struct DropTicket {
    owner: Arc<OwnerState>,
    epoch: u64,
    session: u64,
    request: u64,
}

#[derive(Debug)]
pub(crate) struct OwnerState {
    hwnd: isize,
    epoch: AtomicU64,
    session: AtomicU64,
    accepting: AtomicBool,
    request: AtomicU64,
}

impl OwnerState {
    pub(crate) fn new(hwnd: isize) -> Arc<Self> {
        Arc::new(Self {
            hwnd,
            epoch: AtomicU64::new(1),
            session: AtomicU64::new(0),
            accepting: AtomicBool::new(false),
            request: AtomicU64::new(0),
        })
    }

    pub(crate) fn sync(&self, session: u64, accepting: bool) {
        let was_accepting = self.accepting.load(Ordering::Acquire);
        if self.session.load(Ordering::Acquire) == session && was_accepting == accepting {
            return;
        }
        self.accepting.store(false, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.session.store(session, Ordering::Release);
        self.accepting.store(accepting, Ordering::Release);
    }

    pub(crate) fn retire(&self) {
        self.accepting.store(false, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn ticket(self: &Arc<Self>) -> Result<DropTicket, String> {
        let ticket = DropTicket {
            owner: self.clone(),
            epoch: self.epoch.load(Ordering::Acquire),
            session: self.session.load(Ordering::Acquire),
            request: self.request.fetch_add(1, Ordering::AcqRel) + 1,
        };
        ticket.verify()?;
        Ok(ticket)
    }
}

impl DropTicket {
    /// Full request identity for scoped leases/replies, including the adapter
    /// instance. Reusing an HWND/session and restarting its counter cannot make
    /// an old adapter's Leave equal to the replacement's request.
    pub fn same_request(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
            && self.epoch == other.epoch
            && self.session == other.session
            && self.request == other.request
    }
    pub fn hwnd(&self) -> isize {
        self.owner.hwnd
    }

    pub fn session(&self) -> u64 {
        self.session
    }

    pub fn id(&self) -> u64 {
        self.request
    }

    pub fn is_current(&self) -> bool {
        self.owner.accepting.load(Ordering::Acquire)
            && self.owner.epoch.load(Ordering::Acquire) == self.epoch
            && self.owner.session.load(Ordering::Acquire) == self.session
            && self.owner.request.load(Ordering::Acquire) == self.request
    }

    pub fn verify(&self) -> Result<(), String> {
        if self.is_current() {
            Ok(())
        } else {
            Err("Drag request belongs to a retired window or page.".into())
        }
    }
}

/// Bytes belong to this value after IDataObject::GetData returns. Source COM
/// interfaces, HGLOBAL/HBITMAP and STGMEDIUM never cross to worker threads.
#[derive(Clone, Debug)]
pub struct QuickerActionCandidate {
    pub format_name: String,
    /// Original copied wire, or a format-specific read failure. A failed
    /// candidate never turns an advertised action into ordinary text.
    pub wire: Result<Vec<u8>, String>,
}

#[derive(Clone, Debug, Default)]
pub struct RawDrop {
    pub internal_drag_token: Option<String>,
    pub text: Option<String>,
    pub files: Vec<String>,
    pub html: Option<Vec<u8>>,
    pub chromium_source_url: Option<Vec<u8>>,
    pub qq_rich: Option<Vec<u8>>,
    pub wechat_rich: Option<Vec<u8>>,
    pub quicker_steps: Option<Vec<u8>>,
    /// Compatibility for callers supplying one action format directly.
    pub quicker_action: Option<Vec<u8>>,
    /// Fixed Quicker formats followed by bounded, matching dynamic formats.
    /// The native reader fills this list without duplicating its wire bytes
    /// into the compatibility field above.
    pub quicker_action_candidates: Vec<QuickerActionCandidate>,
    pub png: Option<Vec<u8>>,
    pub dib_v5: Option<Vec<u8>>,
    pub dib: Option<Vec<u8>>,
    pub has_image_format: bool,
}

/// Implement with core's existing private `quicker_wire::decode` and
/// `clipboard::{dib_to_png,dibv5_to_png}` thin public wrappers. No decoder is
/// duplicated here; unsupported binary data must produce a visible error.
pub trait DropPayloadDecoder {
    /// Call core's format-specific reader: CF_HTML UTF-8 first-NUL without
    /// trimming, IM envelope-aware UTF-8/UTF-16, Chromium URL sanitization.
    fn rich_text(&self, name: &str, bytes: &[u8]) -> Result<String, String>;
    fn quicker_action(&self, bytes: &[u8]) -> Result<String, String>;
    fn dib_to_png(&self, bytes: &[u8], v5: bool) -> Result<Vec<u8>, String>;
}

impl RawDrop {
    pub fn is_empty(&self) -> bool {
        self.text.as_ref().is_none_or(String::is_empty)
            && self.files.is_empty()
            && self.html.is_none()
            && self.qq_rich.is_none()
            && self.wechat_rich.is_none()
            && self.quicker_steps.is_none()
            && self.quicker_action.is_none()
            && self.quicker_action_candidates.is_empty()
            && self.png.is_none()
            && self.dib_v5.is_none()
            && self.dib.is_none()
    }

    /// Call on a worker. Only CF_HTML/IM wire terminators/envelopes are decoded;
    /// HTML fragment extraction, embedded images, origin and file references
    /// remain in the production core snapshot-to-record pipeline.
    pub fn into_snapshot(
        self,
        decoder: &impl DropPayloadDecoder,
    ) -> Result<ClipboardSnapshot, String> {
        fn steps(bytes: Option<Vec<u8>>) -> Result<Option<String>, String> {
            bytes
                .map(|bytes| {
                    registered_text::decode(&bytes)
                        .ok_or_else(|| "Invalid Quicker steps drag format.".to_string())
                })
                .transpose()
        }
        let rich = |bytes: Option<Vec<u8>>, name: &str| {
            bytes
                .map(|bytes| decoder.rich_text(name, &bytes))
                .transpose()
        };
        let quicker_action =
            decode_action_candidates(self.quicker_action_candidates, self.quicker_action, decoder)?;
        let png = if let Some(png) = self.png {
            if png.len() < 8 || !png.starts_with(b"\x89PNG\r\n\x1a\n") {
                return Err("Invalid PNG drag format.".into());
            }
            Some(png)
        } else if let Some(dib) = self.dib_v5 {
            Some(decoder.dib_to_png(&dib, true)?)
        } else if let Some(dib) = self.dib {
            Some(decoder.dib_to_png(&dib, false)?)
        } else {
            None
        };
        Ok(ClipboardSnapshot {
            text: self.text,
            files: self.files,
            html: rich(self.html, "HTML Format")?,
            chromium_source_url: rich(self.chromium_source_url, "Chromium internal source URL")?,
            qq_rich: rich(self.qq_rich, "QQ_Unicode_RichEdit_Format")?,
            wechat_rich: rich(self.wechat_rich, "WeChat_RichEdit_Format")?,
            quicker_steps: steps(self.quicker_steps)?,
            quicker_action,
            has_image_format: self.has_image_format || png.is_some(),
            png,
            ..Default::default()
        })
    }
}

fn decode_action_candidates(
    candidates: Vec<QuickerActionCandidate>,
    legacy: Option<Vec<u8>>,
    decoder: &impl DropPayloadDecoder,
) -> Result<Option<String>, String> {
    if candidates.is_empty() {
        return legacy
            .map(|bytes| decoder.quicker_action(&bytes))
            .transpose();
    }
    let mut failures = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let decoded = candidate
            .wire
            .and_then(|bytes| decoder.quicker_action(&bytes));
        match decoded {
            Ok(action) => return Ok(Some(action)),
            Err(error) => failures.push(format!("{}: {error}", candidate.format_name)),
        }
    }
    if let Some(bytes) = legacy {
        match decoder.quicker_action(&bytes) {
            Ok(action) => return Ok(Some(action)),
            Err(error) => failures.push(error),
        }
    }
    Err(format!(
        "No valid Quicker action drag format. {}",
        failures.join("; ")
    ))
}

#[derive(Clone, Debug)]
pub struct DropEvent {
    pub ticket: DropTicket,
    pub control: bool,
    /// Physical screen coordinates, for resolving a destination on the UI
    /// thread. Capture folder/insert index with this event, never on the worker.
    pub screen_point: (i32, i32),
    pub formats: RawDrop,
}

#[derive(Clone, Debug)]
pub struct DropError {
    pub ticket: DropTicket,
    pub message: String,
}

/// Release this exact request's core drag lease, never a later drag's lease.
#[derive(Clone, Debug)]
pub enum DragOverEvent {
    Enter(DropTicket),
    Leave(DropTicket),
}

/// Pure validation for Unicode CF_HDROP. ANSI conversion is done by Windows on
/// the STA. Preserve every path and its order; never check/copy user files here.
pub(crate) fn wide_drop_files(bytes: &[u8]) -> Result<Vec<String>, String> {
    if bytes.len() < 20 || bytes.len() > MAX_FORMAT_BYTES {
        return Err("Invalid file drag header.".into());
    }
    let offset = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    let wide = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) != 0;
    if !wide || offset < 20 || offset % 2 != 0 || offset > bytes.len().saturating_sub(4) {
        return Err("Invalid Unicode file drag offset.".into());
    }
    let mut paths = Vec::new();
    let mut chars = Vec::new();
    for pair in bytes[offset..].chunks_exact(2) {
        let value = u16::from_le_bytes([pair[0], pair[1]]);
        if value == 0 {
            if chars.is_empty() {
                return Ok(paths);
            }
            if paths.len() == MAX_DROP_FILES {
                return Err("Too many dragged files.".into());
            }
            paths.push(String::from_utf16(&chars).map_err(|_| "Invalid file drag Unicode.")?);
            chars.clear();
        } else {
            if chars.len() >= 32767 {
                return Err("Dragged file path is too long.".into());
            }
            chars.push(value);
        }
    }
    Err("File drag is missing its terminator.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Decoder;
    impl DropPayloadDecoder for Decoder {
        fn rich_text(&self, _: &str, bytes: &[u8]) -> Result<String, String> {
            registered_text::decode(bytes).ok_or_else(|| "fixture invalid rich format".into())
        }
        fn quicker_action(&self, _: &[u8]) -> Result<String, String> {
            Err("fixture unsupported".into())
        }
        fn dib_to_png(&self, _: &[u8], _: bool) -> Result<Vec<u8>, String> {
            Err("fixture invalid image".into())
        }
    }

    #[test]
    fn old_page_hidden_and_reused_hwnd_never_revive_tickets() {
        let owner = OwnerState::new(123);
        owner.sync(5, true);
        let old = owner.ticket().unwrap();
        owner.sync(5, true);
        assert!(old.is_current());
        owner.sync(5, false);
        owner.sync(5, true);
        assert!(!old.is_current());
        let next = owner.ticket().unwrap();
        owner.sync(6, true);
        assert!(!next.is_current());
        let current = owner.ticket().unwrap();
        owner.retire();
        assert!(!current.is_current());
        let replacement = OwnerState::new(123);
        replacement.sync(6, true);
        assert!(replacement.ticket().unwrap().is_current());
        assert!(!old.is_current());
    }

    #[test]
    fn later_drag_in_same_window_retires_previous_gesture() {
        let owner = OwnerState::new(123);
        owner.sync(1, true);
        let old = owner.ticket().unwrap();
        let newer = owner.ticket().unwrap();
        assert!(!old.is_current());
        assert!(newer.is_current());
        assert_ne!(old.id(), newer.id());
    }

    #[test]
    fn replacement_adapter_request_cannot_match_an_old_leave_with_reused_numbers() {
        let previous = OwnerState::new(123);
        let replacement = OwnerState::new(123);
        previous.sync(5, true);
        replacement.sync(5, true);
        let old = previous.ticket().unwrap();
        let next = replacement.ticket().unwrap();
        assert_eq!(
            (old.hwnd(), old.session(), old.id()),
            (next.hwnd(), next.session(), next.id())
        );
        assert!(!old.same_request(&next));
        assert!(next.same_request(&next.clone()));
    }

    #[test]
    fn raw_rich_formats_keep_bodies_and_do_not_reduce_to_text() {
        let html = "Version:1.0\r\nSourceURL:https://fixture.invalid/\r\n<b>中文 🦀</b>";
        let qq = "<QQRichEditFormat><EditElement type=\"1\" text=\"中文\" /></QQRichEditFormat>";
        let snapshot = RawDrop {
            text: Some("中文".into()),
            html: Some([html.as_bytes(), &[0]].concat()),
            qq_rich: Some(
                qq.encode_utf16()
                    .chain([0])
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            ),
            files: vec![r"D:\fixture\文件.txt".into()],
            ..Default::default()
        }
        .into_snapshot(&Decoder)
        .unwrap();
        assert_eq!(snapshot.html.as_deref(), Some(html));
        assert_eq!(snapshot.qq_rich.as_deref(), Some(qq));
        assert_eq!(snapshot.text.as_deref(), Some("中文"));
        assert_eq!(snapshot.files, [r"D:\fixture\文件.txt"]);
    }

    #[test]
    fn malformed_registered_or_binary_payload_never_succeeds_as_plain_text() {
        let raw = RawDrop {
            text: Some("fallback".into()),
            html: Some(vec![0xff]),
            ..Default::default()
        };
        assert!(raw.into_snapshot(&Decoder).is_err());
        let raw = RawDrop {
            text: Some("fallback".into()),
            dib: Some(vec![0; 40]),
            ..Default::default()
        };
        assert!(raw.into_snapshot(&Decoder).is_err());
    }

    #[test]
    fn file_drop_keeps_unicode_order_and_rejects_truncated_or_out_of_bounds_data() {
        let paths = [r"D:\中文 🦀.png", r"D:\fixture\folder", r"D:\中文 🦀.png"];
        let mut bytes = vec![0; 20];
        bytes[..4].copy_from_slice(&20u32.to_le_bytes());
        bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
        for path in paths {
            bytes.extend(path.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
        }
        bytes.extend([0; 2]);
        assert_eq!(wide_drop_files(&bytes).unwrap(), paths);
        assert!(wide_drop_files(&bytes[..bytes.len() - 2]).is_err());
        bytes[..4].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
        assert!(wide_drop_files(&bytes).is_err());
    }
}
