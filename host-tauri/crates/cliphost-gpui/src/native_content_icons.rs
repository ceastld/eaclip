//! Step icon dispatch follows the current Web IconControl parser, including its
//! text/emoji/data URI fallbacks. Remote bytes are loaded only by a background
//! worker; this module never writes them to disk or accesses GPUI assets.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepIconSpec {
    FontAwesome(String),
    Shell(String),
    Remote(String),
}

const DEFAULT_STEP_ICON: &str = "fa:Light_ListOl";
const MAX_REMOTE_BYTES: usize = 8 * 1024 * 1024;
const MAX_REDIRECTS: usize = 4;
const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

// ECMAScript WhiteSpace + LineTerminator. Rust's Unicode trim includes U+0085
// and excludes U+FEFF, so it changes the Web parser's prefix/fill boundaries.
fn js_whitespace(ch: char) -> bool {
    matches!(ch, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
        | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(js_whitespace)
}

/// Equivalent to parseFavorIconSpec(raw, "StepClipItem"). HTTP prefixes are
/// case-sensitive; fa:/text:/url:/icon: prefixes are not. `text:` intentionally
/// selects the kind fallback rather than rendering a label. Bare emoji and
/// data:image stay Shell specs, as in Web; they are not decoded here.
pub fn parse_step_icon(raw: &str) -> StepIconSpec {
    let text = js_trim(raw);
    if text.is_empty() || has_prefix(text, "text:") {
        return StepIconSpec::FontAwesome(DEFAULT_STEP_ICON.to_owned());
    }
    if is_font_awesome(text) {
        return StepIconSpec::FontAwesome(if has_prefix(text, "fa:") {
            text.to_owned()
        } else {
            format!("fa:{text}")
        });
    }
    let candidate = if has_prefix(text, "url:") {
        js_trim(&text[4..])
    } else {
        text
    };
    if candidate.starts_with("http://") || candidate.starts_with("https://") {
        return StepIconSpec::Remote(candidate.to_owned());
    }
    if candidate.starts_with("//") {
        return StepIconSpec::Remote(format!("https:{candidate}"));
    }
    StepIconSpec::Shell(if has_prefix(text, "icon:") {
        js_trim(&text[5..]).to_owned()
    } else {
        text.to_owned()
    })
}

fn has_prefix(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|part| part.eq_ignore_ascii_case(prefix))
}

fn is_font_awesome(value: &str) -> bool {
    let value = if has_prefix(value, "fa:") {
        &value[3..]
    } else {
        value
    };
    let (name, fill) = value
        .split_once(':')
        .map_or((value, None), |(name, fill)| (name, Some(fill)));
    if fill.is_some_and(|fill| fill.is_empty() || fill.contains(':')) {
        return false;
    }
    let Some((style, icon)) = name.split_once('_') else {
        return false;
    };
    ["Light", "Solid", "Regular", "Brands"]
        .iter()
        .any(|known| style.eq_ignore_ascii_case(known))
        && !icon.is_empty()
        && icon
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_')
}

/// FE embedded cache lookup is case-sensitive even though its FA parser and
/// the host catalog accept mixed-case names. This list contains names only.
pub fn is_embedded_fa(spec: &str) -> bool {
    let mut text = js_trim(spec);
    if has_prefix(text, "fa:") {
        text = js_trim(&text[3..]);
    }
    if let Some(at) = text.find(':').filter(|at| *at > 0) {
        text = js_trim(&text[..at]);
    }
    matches!(
        text,
        "Light_Text"
            | "Light_File"
            | "Light_Image"
            | "Light_Code"
            | "Light_Comment"
            | "Light_Link"
            | "Light_Folder"
            | "Light_ListOl"
            | "Light_Bolt"
            | "Light_Scroll"
            | "Light_Edit"
            | "Light_Keyboard"
            | "Light_Calculator"
            | "Solid_Cubes"
    )
}

/// FE parseFaSpecFill: trim the raw suffix after the first colon following an
/// optional fa: prefix. No semantic mapping or color validation occurs here.
pub fn explicit_fa_fill(spec: &str) -> Option<String> {
    let mut text = js_trim(spec);
    if has_prefix(text, "fa:") {
        text = js_trim(&text[3..]);
    }
    let at = text.find(':')?;
    if at == 0 || at == text.len() - 1 {
        return None;
    }
    let fill = js_trim(&text[at + 1..]);
    (!fill.is_empty()).then(|| fill.to_owned())
}

/// Background-only bounded HTTP(S) download. The Windows implementation uses
/// WinHTTP with a total deadline, bounded redirects, and an 8 MiB body limit.
pub fn fetch_remote_icon(url: &str) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        windows_http::fetch(url)
    }
    #[cfg(not(windows))]
    {
        let _ = url;
        Err("Remote step icons require Windows WinHTTP".to_owned())
    }
}

const MAX_URL_BYTES: usize = 16 * 1024;

fn remote_url(raw: &str) -> Result<url::Url, String> {
    let raw = raw.trim();
    if raw.len() > MAX_URL_BYTES || raw.contains('\0') {
        return Err("Invalid remote icon URL".to_owned());
    }
    let normalized = if raw.starts_with("//") {
        format!("https:{raw}")
    } else {
        raw.to_owned()
    };
    let mut url = url::Url::parse(&normalized).map_err(|_| "Invalid remote icon URL".to_owned())?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err("Remote icons require an HTTP or HTTPS URL".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Remote icon URL credentials are unsupported".to_owned());
    }
    url.set_fragment(None);
    Ok(url)
}

fn redirected_url(current: &url::Url, location: &str, count: usize) -> Result<url::Url, String> {
    if count >= MAX_REDIRECTS {
        return Err("Remote icon redirect limit exceeded".to_owned());
    }
    if location.len() > MAX_URL_BYTES || location.trim().is_empty() || location.contains('\0') {
        return Err("Invalid remote icon redirect".to_owned());
    }
    let next = current
        .join(location.trim())
        .map_err(|_| "Invalid remote icon redirect".to_owned())?;
    remote_url(next.as_str())
}

fn append_body(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), String> {
    if body
        .len()
        .checked_add(chunk.len())
        .is_none_or(|size| size > MAX_REMOTE_BYTES)
    {
        return Err("Remote icon exceeds 8 MiB".to_owned());
    }
    body.try_reserve(chunk.len())
        .map_err(|_| "Remote icon allocation failed".to_owned())?;
    body.extend_from_slice(chunk);
    Ok(())
}

fn check_content_length(value: Option<&str>) -> Result<(), String> {
    if let Some(value) = value {
        let length = value
            .trim()
            .parse::<u64>()
            .map_err(|_| "Invalid remote icon Content-Length".to_owned())?;
        if length > MAX_REMOTE_BYTES as u64 {
            return Err("Remote icon exceeds 8 MiB".to_owned());
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Deadline {
    started: std::time::Instant,
    budget: std::time::Duration,
}

impl Deadline {
    fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            budget: DOWNLOAD_TIMEOUT,
        }
    }

    fn remaining(&self) -> Result<std::time::Duration, String> {
        self.budget
            .checked_sub(self.started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| "Remote icon download deadline exceeded".to_owned())
    }
}

#[cfg(windows)]
mod windows_http {
    use std::{
        cell::UnsafeCell,
        ffi::c_void,
        mem::size_of,
        ptr,
        sync::{
            mpsc::{self, Receiver, SyncSender},
            Arc,
        },
    };

    use super::{append_body, check_content_length, redirected_url, remote_url, Deadline};

    // Signatures/constants mirror windows 0.61.3's official WinHttp bindings.
    // Narrow FFI avoids changing the product's Windows dependency features.
    type Handle = *mut c_void;
    type Callback = Option<unsafe extern "system" fn(Handle, usize, u32, *mut c_void, u32)>;
    #[link(name = "winhttp")]
    unsafe extern "system" {
        fn WinHttpOpen(
            agent: *const u16,
            access: u32,
            proxy: *const u16,
            bypass: *const u16,
            flags: u32,
        ) -> Handle;
        fn WinHttpConnect(session: Handle, server: *const u16, port: u16, reserved: u32) -> Handle;
        fn WinHttpOpenRequest(
            connection: Handle,
            verb: *const u16,
            object: *const u16,
            version: *const u16,
            referrer: *const u16,
            accept: *const *const u16,
            flags: u32,
        ) -> Handle;
        fn WinHttpSetOption(handle: Handle, option: u32, buffer: *const c_void, bytes: u32) -> i32;
        fn WinHttpSetTimeouts(
            handle: Handle,
            resolve: i32,
            connect: i32,
            send: i32,
            receive: i32,
        ) -> i32;
        fn WinHttpSetStatusCallback(
            handle: Handle,
            callback: Callback,
            flags: u32,
            reserved: usize,
        ) -> usize;
        fn WinHttpSendRequest(
            request: Handle,
            headers: *const u16,
            header_length: u32,
            optional: *const c_void,
            optional_length: u32,
            total_length: u32,
            context: usize,
        ) -> i32;
        fn WinHttpReceiveResponse(request: Handle, reserved: *mut c_void) -> i32;
        fn WinHttpQueryHeaders(
            request: Handle,
            level: u32,
            name: *const u16,
            buffer: *mut c_void,
            bytes: *mut u32,
            index: *mut u32,
        ) -> i32;
        fn WinHttpReadData(
            request: Handle,
            buffer: *mut c_void,
            length: u32,
            read: *mut u32,
        ) -> i32;
        fn WinHttpCloseHandle(handle: Handle) -> i32;
    }

    const ASYNC: u32 = 0x1000_0000;
    const SECURE: u32 = 0x0080_0000;
    const AUTOMATIC_PROXY: u32 = 4;
    const CONTEXT_VALUE: u32 = 45;
    const DISABLE_FEATURE: u32 = 63;
    const MAX_HEADER_SIZE: u32 = 91;
    const DECOMPRESSION: u32 = 118;
    const STATUS_CODE: u32 = 19;
    const LOCATION: u32 = 33;
    const CONTENT_LENGTH: u32 = 5;
    const NUMBER: u32 = 0x2000_0000;
    const HANDLE_CLOSING: u32 = 0x0000_0800;
    const SEND_COMPLETE: u32 = 0x0040_0000;
    const HEADERS_AVAILABLE: u32 = 0x0002_0000;
    const READ_COMPLETE: u32 = 0x0008_0000;
    const REQUEST_ERROR: u32 = 0x0020_0000;
    const CHUNK_BYTES: usize = 32 * 1024;
    const HEADER_BYTES: u32 = 64 * 1024;
    const HEADER_NOT_FOUND: i32 = 12150;
    const INSUFFICIENT_BUFFER: i32 = 122;

    struct OwnedHandle(Handle);
    impl OwnedHandle {
        fn new(raw: Handle, operation: &str) -> Result<Self, String> {
            if raw.is_null() {
                Err(last_error(operation))
            } else {
                Ok(Self(raw))
            }
        }
    }
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                WinHttpCloseHandle(self.0);
            }
        }
    }

    enum Event {
        Sent,
        Headers,
        Chunk(Vec<u8>),
        Error(u32),
    }

    struct Context {
        events: SyncSender<Event>,
        buffer: UnsafeCell<[u8; CHUNK_BYTES]>,
    }

    // One read is submitted at a time. WinHTTP writes only while that read is
    // pending; the callback copies its completed bytes before sending the event.
    // The callback-owned Arc keeps the buffer alive until HANDLE_CLOSING, even
    // if the waiting worker times out and returns before cancellation finishes.
    unsafe impl Sync for Context {}

    #[repr(C)]
    struct AsyncResult {
        result: usize,
        error: u32,
    }

    unsafe extern "system" fn status_callback(
        _request: Handle,
        context: usize,
        status: u32,
        information: *mut c_void,
        length: u32,
    ) {
        if context == 0 {
            return;
        }
        let pointer = context as *const Context;
        if status == HANDLE_CLOSING {
            // Final notification: release precisely the Arc registered as the
            // native callback context. Request's own Arc may already be gone.
            unsafe {
                drop(Arc::from_raw(pointer));
            }
            return;
        }
        let state = unsafe { &*pointer };
        let event = match status {
            SEND_COMPLETE => Event::Sent,
            HEADERS_AVAILABLE => Event::Headers,
            READ_COMPLETE if length == 0 => Event::Chunk(Vec::new()),
            READ_COMPLETE if !information.is_null() && length as usize <= CHUNK_BYTES => {
                let bytes = unsafe {
                    std::slice::from_raw_parts(information as *const u8, length as usize)
                };
                Event::Chunk(bytes.to_vec())
            }
            REQUEST_ERROR
                if !information.is_null() && length as usize >= size_of::<AsyncResult>() =>
            {
                let result = unsafe { ptr::read_unaligned(information as *const AsyncResult) };
                Event::Error(result.error)
            }
            READ_COMPLETE | REQUEST_ERROR => Event::Error(13),
            _ => return,
        };
        // Never block a WinHTTP callback. The channel has space for all
        // completion/error events of the single outstanding operation.
        let _ = state.events.try_send(event);
    }

    struct Request {
        handle: OwnedHandle,
        context: Arc<Context>,
        events: Receiver<Event>,
    }

    impl Request {
        fn new(connection: &OwnedHandle, target: &str, secure: bool) -> Result<Self, String> {
            let verb = wide("GET");
            let target = wide(target);
            let handle = OwnedHandle::new(
                unsafe {
                    WinHttpOpenRequest(
                        connection.0,
                        verb.as_ptr(),
                        target.as_ptr(),
                        ptr::null(),
                        ptr::null(),
                        ptr::null(),
                        if secure { SECURE } else { 0 },
                    )
                },
                "open request",
            )?;
            // Disable cookies, automatic authentication and automatic redirects.
            // Redirects below are followed explicitly under the same deadline.
            option_u32(handle.0, DISABLE_FEATURE, 1 | 2 | 4)?;
            option_u32(handle.0, MAX_HEADER_SIZE, HEADER_BYTES)?;
            option_u32(handle.0, DECOMPRESSION, 1 | 2)?;
            let (events, receiver) = mpsc::sync_channel(4);
            let context = Arc::new(Context {
                events,
                buffer: UnsafeCell::new([0; CHUNK_BYTES]),
            });
            let pointer = Arc::into_raw(context.clone());
            let value = pointer as usize;
            let set = unsafe {
                WinHttpSetOption(
                    handle.0,
                    CONTEXT_VALUE,
                    &value as *const usize as *const c_void,
                    size_of::<usize>() as u32,
                )
            };
            if set == 0 {
                let error = last_error("set callback context");
                unsafe {
                    drop(Arc::from_raw(pointer));
                }
                return Err(error);
            }
            let previous = unsafe {
                WinHttpSetStatusCallback(
                    handle.0,
                    Some(status_callback),
                    0x0000_0c00 | SEND_COMPLETE | HEADERS_AVAILABLE | READ_COMPLETE | REQUEST_ERROR,
                    0,
                )
            };
            if previous == usize::MAX {
                let error = last_error("set status callback");
                unsafe {
                    drop(Arc::from_raw(pointer));
                }
                return Err(error);
            }
            Ok(Self {
                handle,
                context,
                events: receiver,
            })
        }

        fn wait(&self, deadline: &Deadline) -> Result<Event, String> {
            match self.events.recv_timeout(deadline.remaining()?) {
                Ok(Event::Error(error)) => Err(format!("Remote icon WinHTTP error {error}")),
                Ok(event) => Ok(event),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    Err("Remote icon download deadline exceeded".to_owned())
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err("Remote icon request closed".to_owned())
                }
            }
        }

        fn send(&self, deadline: &Deadline) -> Result<(), String> {
            deadline.remaining()?;
            let headers = wide("Accept: image/*\r\nAccept-Encoding: gzip, deflate\r\n");
            let result = unsafe {
                WinHttpSendRequest(
                    self.handle.0,
                    headers.as_ptr(),
                    (headers.len() - 1) as u32,
                    ptr::null(),
                    0,
                    0,
                    Arc::as_ptr(&self.context) as usize,
                )
            };
            checked(result, "send request")?;
            if !matches!(self.wait(deadline)?, Event::Sent) {
                return Err("Unexpected remote icon send completion".to_owned());
            }
            deadline.remaining()?;
            checked(
                unsafe { WinHttpReceiveResponse(self.handle.0, ptr::null_mut()) },
                "receive response",
            )?;
            if !matches!(self.wait(deadline)?, Event::Headers) {
                return Err("Unexpected remote icon response completion".to_owned());
            }
            Ok(())
        }

        fn status(&self) -> Result<u32, String> {
            let mut code = 0u32;
            let mut bytes = size_of::<u32>() as u32;
            checked(
                unsafe {
                    WinHttpQueryHeaders(
                        self.handle.0,
                        STATUS_CODE | NUMBER,
                        ptr::null(),
                        &mut code as *mut u32 as *mut c_void,
                        &mut bytes,
                        ptr::null_mut(),
                    )
                },
                "query status",
            )?;
            Ok(code)
        }

        fn header(&self, level: u32) -> Result<Option<String>, String> {
            let mut bytes = 0u32;
            let result = unsafe {
                WinHttpQueryHeaders(
                    self.handle.0,
                    level,
                    ptr::null(),
                    ptr::null_mut(),
                    &mut bytes,
                    ptr::null_mut(),
                )
            };
            if result == 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(HEADER_NOT_FOUND) {
                    return Ok(None);
                }
                if error.raw_os_error() != Some(INSUFFICIENT_BUFFER) {
                    return Err(format!("Remote icon query header: {error}"));
                }
            }
            if bytes > HEADER_BYTES || bytes % 2 != 0 {
                return Err("Remote icon response header exceeds limit".to_owned());
            }
            let mut buffer = vec![0u16; bytes as usize / 2 + 1];
            if bytes == 0 {
                return Ok(Some(String::new()));
            }
            checked(
                unsafe {
                    WinHttpQueryHeaders(
                        self.handle.0,
                        level,
                        ptr::null(),
                        buffer.as_mut_ptr() as *mut c_void,
                        &mut bytes,
                        ptr::null_mut(),
                    )
                },
                "query header",
            )?;
            let end = buffer
                .iter()
                .position(|ch| *ch == 0)
                .unwrap_or(buffer.len());
            Ok(Some(String::from_utf16_lossy(&buffer[..end])))
        }

        fn body(&self, deadline: &Deadline) -> Result<Vec<u8>, String> {
            check_content_length(self.header(CONTENT_LENGTH)?.as_deref())?;
            let mut body = Vec::new();
            loop {
                deadline.remaining()?;
                checked(
                    unsafe {
                        WinHttpReadData(
                            self.handle.0,
                            self.context.buffer.get() as *mut c_void,
                            CHUNK_BYTES as u32,
                            ptr::null_mut(),
                        )
                    },
                    "read body",
                )?;
                let Event::Chunk(chunk) = self.wait(deadline)? else {
                    return Err("Unexpected remote icon read completion".to_owned());
                };
                if chunk.is_empty() {
                    return Ok(body);
                }
                append_body(&mut body, &chunk)?;
            }
        }
    }

    pub(super) fn fetch(raw: &str) -> Result<Vec<u8>, String> {
        let deadline = Deadline::new();
        let mut url = remote_url(raw)?;
        let agent = wide("ClipHost/StepIcon");
        let session = OwnedHandle::new(
            unsafe {
                WinHttpOpen(
                    agent.as_ptr(),
                    AUTOMATIC_PROXY,
                    ptr::null(),
                    ptr::null(),
                    ASYNC,
                )
            },
            "open session",
        )?;
        checked(
            unsafe { WinHttpSetTimeouts(session.0, 4_000, 4_000, 4_000, 4_000) },
            "set timeouts",
        )?;
        for redirects in 0..=super::MAX_REDIRECTS {
            deadline.remaining()?;
            let host = match url
                .host()
                .ok_or_else(|| "Missing remote icon host".to_owned())?
            {
                url::Host::Domain(host) => host.to_owned(),
                url::Host::Ipv4(host) => host.to_string(),
                url::Host::Ipv6(host) => host.to_string(),
            };
            let host = wide(&host);
            let port = url
                .port_or_known_default()
                .ok_or_else(|| "Missing remote icon port".to_owned())?;
            let connection = OwnedHandle::new(
                unsafe { WinHttpConnect(session.0, host.as_ptr(), port, 0) },
                "connect",
            )?;
            let mut target = url.path().to_owned();
            if let Some(query) = url.query() {
                target.push('?');
                target.push_str(query);
            }
            let request = Request::new(&connection, &target, url.scheme() == "https")?;
            request.send(&deadline)?;
            deadline.remaining()?;
            let status = request.status()?;
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let location = request
                    .header(LOCATION)?
                    .ok_or_else(|| "Remote icon redirect has no Location".to_owned())?;
                url = redirected_url(&url, &location, redirects)?;
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(format!("Remote icon HTTP {status}"));
            }
            return request.body(&deadline);
        }
        Err("Remote icon redirect limit exceeded".to_owned())
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn option_u32(handle: Handle, option: u32, value: u32) -> Result<(), String> {
        checked(
            unsafe {
                WinHttpSetOption(
                    handle,
                    option,
                    &value as *const u32 as *const c_void,
                    size_of::<u32>() as u32,
                )
            },
            "set option",
        )
    }

    fn checked(result: i32, operation: &str) -> Result<(), String> {
        if result == 0 {
            Err(last_error(operation))
        } else {
            Ok(())
        }
    }

    fn last_error(operation: &str) -> String {
        format!(
            "Remote icon {operation}: {}",
            std::io::Error::last_os_error()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_offline_runner_icons_preserve_names_and_explicit_colors() {
        // quicker_steps.rs offline_runner produces these actual Step DTO specs.
        for raw in [
            "fa:Light_Scroll:#6aaded",
            "fa:Solid_Cubes:#3196F4",
            "fa:Light_Edit:#6aaded",
            "fa:Light_Code",
            "fa:Light_Bullhorn:#6aaded",
            "fa:Light_Cog:#6aaded",
            "fa:Light_Keyboard:#6aaded",
            "fa:Light_Calculator:#6aaded",
        ] {
            assert_eq!(
                parse_step_icon(raw),
                StepIconSpec::FontAwesome(raw.to_owned())
            );
        }
    }

    #[test]
    fn web_fa_prefix_style_word_and_empty_color_boundaries_are_exact() {
        for (raw, expected) in [
            (" Light_Text ", "fa:Light_Text"),
            ("FA:light_text: RED", "FA:light_text: RED"),
            ("brands_Github", "fa:brands_Github"),
            ("Light__123", "fa:Light__123"),
            ("fa:Light_Text:rgb(1, 2, 3)", "fa:Light_Text:rgb(1, 2, 3)"),
        ] {
            assert_eq!(
                parse_step_icon(raw),
                StepIconSpec::FontAwesome(expected.to_owned())
            );
        }
        for raw in [
            "fa: Light_Text",
            "fa:Light_Text:",
            "fa:Light_Text:   ",
            "fa:Light_Text:red:blue",
            "Light_中文",
            "Light_é",
            "Solid_File-Alt",
            "Light_",
            "fa:bad",
        ] {
            assert_eq!(
                parse_step_icon(raw),
                StepIconSpec::Shell(raw.trim().to_owned())
            );
        }
    }

    #[test]
    fn web_trim_removes_bom_at_outer_and_prefix_suffix_boundaries() {
        assert_eq!(
            parse_step_icon("\u{feff}fa:Light_Text\u{feff}"),
            StepIconSpec::FontAwesome("fa:Light_Text".to_owned())
        );
        assert_eq!(
            parse_step_icon("URL:\u{feff}https://example.test/icon\u{feff}"),
            StepIconSpec::Remote("https://example.test/icon".to_owned())
        );
        assert_eq!(
            parse_step_icon("ICON:\u{feff}C:\\a.exe\u{feff}"),
            StepIconSpec::Shell("C:\\a.exe".to_owned())
        );
        assert!(is_embedded_fa(
            "\u{feff}FA:\u{feff}Light_Text\u{feff}:red\u{feff}"
        ));
        assert_eq!(
            explicit_fa_fill("\u{feff}FA:\u{feff}Light_Text:\u{feff}success\u{feff}"),
            Some("success".to_owned())
        );
        // FA_PATTERN itself does not trim inside the prefix/name boundary.
        assert_eq!(
            parse_step_icon("fa:\u{feff}Light_Text"),
            StepIconSpec::Shell("fa:\u{feff}Light_Text".to_owned())
        );
    }

    #[test]
    fn web_trim_keeps_next_line_and_zero_width_space() {
        for ch in ['\u{0085}', '\u{200b}'] {
            let raw = format!("{ch}fa:Light_Text{ch}");
            assert_eq!(parse_step_icon(&raw), StepIconSpec::Shell(raw.clone()));
            assert!(!is_embedded_fa(&raw));
            let url = format!("url:{ch}https://example.test/icon");
            assert_eq!(parse_step_icon(&url), StepIconSpec::Shell(url.clone()));
            let icon = format!("icon:{ch}C:\\a.exe{ch}");
            assert_eq!(
                parse_step_icon(&icon),
                StepIconSpec::Shell(format!("{ch}C:\\a.exe{ch}"))
            );
            let colored = format!("fa:Light_Text:{ch}");
            assert_eq!(
                parse_step_icon(&colored),
                StepIconSpec::FontAwesome(colored.clone())
            );
            assert_eq!(explicit_fa_fill(&colored), Some(ch.to_string()));
        }
    }

    #[test]
    fn text_and_emoji_labels_follow_current_web_fallbacks() {
        for raw in [
            "",
            "  ",
            "text:",
            "TEXT:字",
            "text: 😀 ",
            "text:👩🏽‍💻:#fff",
            "text:hello:Red",
        ] {
            assert_eq!(
                parse_step_icon(raw),
                StepIconSpec::FontAwesome(DEFAULT_STEP_ICON.to_owned())
            );
        }
        for raw in [
            "😀",
            "emoji:😀",
            "Emoji:👩🏽‍💻",
            "data:image/png;base64,iVBORw0KGgo=",
            "url:data:image/svg+xml,<svg/>",
            "remote:https://example.test/a.png",
        ] {
            assert_eq!(parse_step_icon(raw), StepIconSpec::Shell(raw.to_owned()));
        }
    }

    #[test]
    fn urls_and_shell_prefixes_match_web_case_and_original_spec_rules() {
        for (raw, expected) in [
            ("https://example.test/icon", "https://example.test/icon"),
            (
                "http://example.test/a.svg?x=1",
                "http://example.test/a.svg?x=1",
            ),
            ("URL: https://example.test/a", "https://example.test/a"),
            ("//example.test/a.png", "https://example.test/a.png"),
            ("url: //example.test/a", "https://example.test/a"),
            ("http://", "http://"),
        ] {
            assert_eq!(
                parse_step_icon(raw),
                StepIconSpec::Remote(expected.to_owned())
            );
        }
        for raw in [
            "HTTP://example.test/a.png",
            "Url: HTTPS://example.test/a",
            "url:local.png",
            "file:///C:/a.png",
        ] {
            assert_eq!(parse_step_icon(raw), StepIconSpec::Shell(raw.to_owned()));
        }
        assert_eq!(
            parse_step_icon("ICON:  C:\\a.exe "),
            StepIconSpec::Shell("C:\\a.exe".to_owned())
        );
        assert_eq!(parse_step_icon("icon:"), StepIconSpec::Shell(String::new()));
    }

    #[test]
    fn embedded_name_case_and_raw_fill_are_separate_from_host_semantic_mapping() {
        assert!(is_embedded_fa("FA: Light_Text :success"));
        assert!(!is_embedded_fa("fa:light_text:success"));
        assert!(!is_embedded_fa("fa:Light_Bullhorn:#6aaded"));
        assert!(!is_embedded_fa("fa:Light_Cog:#6aaded"));
        assert_eq!(
            explicit_fa_fill("fa:Light_Text: success "),
            Some("success".to_owned())
        );
        assert_eq!(
            explicit_fa_fill("FA:light_text:SUCCESS"),
            Some("SUCCESS".to_owned())
        );
        assert_eq!(
            explicit_fa_fill("Light_Text: rgb(1, 2, 3)"),
            Some("rgb(1, 2, 3)".to_owned())
        );
        // Helpers mirror panelFaGlyphs independently of the stricter parser.
        assert_eq!(
            explicit_fa_fill("fa:Light_Text:red:blue"),
            Some("red:blue".to_owned())
        );
        for raw in [
            "",
            "fa:Light_Text",
            "fa:Light_Text:",
            "fa:Light_Text:   ",
            "fa::red",
        ] {
            assert_eq!(explicit_fa_fill(raw), None);
        }
        for name in [
            "Light_Text",
            "Light_File",
            "Light_Image",
            "Light_Code",
            "Light_Comment",
            "Light_Link",
            "Light_Folder",
            "Light_ListOl",
            "Light_Bolt",
            "Light_Scroll",
            "Light_Edit",
            "Light_Keyboard",
            "Light_Calculator",
            "Solid_Cubes",
        ] {
            assert!(is_embedded_fa(name), "{name}");
        }
    }

    #[test]
    fn remote_validation_and_redirect_limit_cannot_escape_http_schemes() {
        assert_eq!(
            remote_url("//example.test/a#fragment").unwrap().as_str(),
            "https://example.test/a"
        );
        let base = remote_url("https://example.test/a/b").unwrap();
        assert_eq!(
            redirected_url(&base, "../icon.png", 0).unwrap().as_str(),
            "https://example.test/icon.png"
        );
        assert!(redirected_url(&base, "//other.test/icon", MAX_REDIRECTS - 1).is_ok());
        assert!(redirected_url(&base, "/icon", MAX_REDIRECTS).is_err());
        for raw in [
            "file:///C:/a.png",
            "data:image/png;base64,eA==",
            "http://",
            "https://user:pass@example.test/a",
            "https://example.test/\0",
        ] {
            assert!(remote_url(raw).is_err(), "{raw}");
        }
        for location in [
            "file:///C:/a.png",
            "data:image/png;base64,eA==",
            "https://user@example.test/a",
            "",
            "\0",
        ] {
            assert!(redirected_url(&base, location, 0).is_err(), "{location}");
        }
        assert!(remote_url(&format!(
            "https://example.test/{}",
            "x".repeat(MAX_URL_BYTES)
        ))
        .is_err());
    }

    #[test]
    fn exact_body_limit_and_untrusted_content_length_are_bounded() {
        check_content_length(None).unwrap();
        check_content_length(Some(&MAX_REMOTE_BYTES.to_string())).unwrap();
        assert!(check_content_length(Some(&(MAX_REMOTE_BYTES + 1).to_string())).is_err());
        assert!(check_content_length(Some("18446744073709551616")).is_err());
        assert!(check_content_length(Some("invalid")).is_err());
        let mut body = vec![0; MAX_REMOTE_BYTES - 1];
        append_body(&mut body, &[1]).unwrap();
        assert_eq!(body.len(), MAX_REMOTE_BYTES);
        assert!(append_body(&mut body, &[2]).is_err());
        assert_eq!(body.len(), MAX_REMOTE_BYTES);
    }

    #[test]
    fn deadline_is_shared_across_all_operations_and_never_restarts() {
        let deadline = Deadline::new();
        assert!(deadline.remaining().unwrap() <= DOWNLOAD_TIMEOUT);
        let expired = Deadline {
            started: std::time::Instant::now(),
            budget: std::time::Duration::ZERO,
        };
        assert!(expired.remaining().is_err());
        assert_eq!(DOWNLOAD_TIMEOUT.as_secs(), 15);
    }
}
