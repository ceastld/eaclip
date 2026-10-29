//! Best-effort evidence for a small set of unhandled native Windows faults.
//!
//! This is not a fault-stack unwinder or a recovery handler. Stack overflow,
//! heap corruption, fail-fast, language exceptions and unknown codes retain
//! normal Windows handling. Fast-fail/abort need not reach this filter at all.
//! Install only during healthy startup, after attempting to create startup JSONL. The
//! callback must not use the Rust allocator, logger, locks, serde or DbgHelp.

use std::{io, path::Path};

/// Prepare a separate append handle and message before registering the filter.
/// `allow_dialog` must already reflect fixture/headless/native-acceptance mode.
/// A log error is returned after the filter is registered: a missing/unwritable
/// log must not prevent the best-effort error dialog.
pub(crate) fn install(path: Option<&Path>, allow_dialog: bool) -> io::Result<()> {
    #[cfg(windows)]
    return windows_impl::install(path, allow_dialog);
    #[cfg(not(windows))]
    {
        let _ = (path, allow_dialog);
        Ok(())
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::{
        cell::UnsafeCell,
        ffi::c_void,
        fs::OpenOptions,
        io,
        os::windows::{ffi::OsStrExt, io::IntoRawHandle},
        path::Path,
        ptr,
        sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering},
    };

    const CONTINUE_SEARCH: i32 = 0;
    const ACCESS_VIOLATION: u32 = 0xc000_0005;
    const ILLEGAL_INSTRUCTION: u32 = 0xc000_001d;
    const INTEGER_DIVIDE_BY_ZERO: u32 = 0xc000_0094;
    const INTEGER_OVERFLOW: u32 = 0xc000_0095;
    const PRIVILEGED_INSTRUCTION: u32 = 0xc000_0096;
    const MESSAGE_CAPACITY: usize = 2048;

    struct MessageStorage(UnsafeCell<[u16; MESSAGE_CAPACITY]>);
    // Initialization finishes before SetUnhandledExceptionFilter publishes the
    // callback. ENTERED admits at most one callback writer for the process.
    unsafe impl Sync for MessageStorage {}

    static INSTALLED: AtomicBool = AtomicBool::new(false);
    static READY: AtomicBool = AtomicBool::new(false);
    static ENTERED: AtomicBool = AtomicBool::new(false);
    static DELEGATING: AtomicBool = AtomicBool::new(false);
    static PREVIOUS_FILTER: AtomicUsize = AtomicUsize::new(0);
    static APPEND_HANDLE: AtomicIsize = AtomicIsize::new(0);
    static IMAGE_BASE: AtomicUsize = AtomicUsize::new(0);
    static ALLOW_DIALOG: AtomicBool = AtomicBool::new(false);
    static CODE_OFFSET: AtomicUsize = AtomicUsize::new(0);
    static ADDRESS_OFFSET: AtomicUsize = AtomicUsize::new(0);
    static MESSAGE_WRITTEN: MessageStorage = MessageStorage(UnsafeCell::new([0; MESSAGE_CAPACITY]));
    static MESSAGE_UNWRITTEN: MessageStorage =
        MessageStorage(UnsafeCell::new([0; MESSAGE_CAPACITY]));
    const TITLE: [u16; 22] = ascii_wide(b"ClipHost native error\0");

    const fn ascii_wide<const N: usize>(bytes: &[u8; N]) -> [u16; N] {
        let mut result = [0; N];
        let mut index = 0;
        while index < N {
            result[index] = bytes[index] as u16;
            index += 1;
        }
        result
    }

    #[repr(C)]
    struct ExceptionRecord {
        code: u32,
        flags: u32,
        nested: *mut ExceptionRecord,
        address: *mut c_void,
        parameter_count: u32,
        information: [usize; 15],
    }

    #[repr(C)]
    struct ExceptionPointers {
        record: *mut ExceptionRecord,
        context: *mut c_void,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    type Filter = unsafe extern "system" fn(*mut ExceptionPointers) -> i32;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetUnhandledExceptionFilter(filter: Option<Filter>) -> Option<Filter>;
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
        fn GetCurrentProcessId() -> u32;
        fn GetCurrentThreadId() -> u32;
        fn GetSystemTime(time: *mut SystemTime);
        fn WriteFile(
            file: *mut c_void,
            buffer: *const c_void,
            length: u32,
            written: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(
            owner: *mut c_void,
            text: *const u16,
            caption: *const u16,
            flags: u32,
        ) -> i32;
    }

    pub(super) fn install(path: Option<&Path>, allow_dialog: bool) -> io::Result<()> {
        if INSTALLED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        let file = match path {
            Some(path) => OpenOptions::new().append(true).open(path),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "startup log path unavailable",
            )),
        };
        let mut message = [0_u16; MESSAGE_CAPACITY];
        let mut position = 0;
        append_wide(
            &mut message,
            &mut position,
            "ClipHost 遇到无法继续的原生异常。\n\n异常代码：0x".encode_utf16(),
        );
        let code_offset = position;
        append_wide(
            &mut message,
            &mut position,
            "00000000\n异常地址：0x".encode_utf16(),
        );
        let address_offset = position;
        append_wide(
            &mut message,
            &mut position,
            "0000000000000000\n\n".encode_utf16(),
        );
        let mut message_unwritten = message;
        let mut unwritten_position = position;
        append_wide(
            &mut message,
            &mut position,
            "本次异常已写入启动日志：\n".encode_utf16(),
        );
        if let Some(path) = path {
            append_wide(&mut message, &mut position, path.as_os_str().encode_wide());
        }
        append_wide(
            &mut message,
            &mut position,
            "\n\n程序即将退出。请将日志发给开发者。\n按 Ctrl+C 可复制此错误提示。\n原生故障未采集 Rust 调用栈。".encode_utf16(),
        );
        append_wide(
            &mut message_unwritten,
            &mut unwritten_position,
            "本次原生异常未能写入日志。\n按 Ctrl+C 可复制此错误提示并反馈，也可截图。\n原生故障未采集 Rust 调用栈。\n程序即将退出。".encode_utf16(),
        );

        // The owned append handle deliberately lasts until process termination.
        // A single WriteFile uses FILE_APPEND_DATA without a Rust file mutex.
        let log_result = match file {
            Ok(file) => {
                APPEND_HANDLE.store(file.into_raw_handle() as isize, Ordering::Release);
                Ok(())
            }
            Err(error) => Err(error),
        };
        IMAGE_BASE.store(
            unsafe { GetModuleHandleW(ptr::null()) } as usize,
            Ordering::Release,
        );
        ALLOW_DIALOG.store(allow_dialog, Ordering::Release);
        CODE_OFFSET.store(code_offset, Ordering::Release);
        ADDRESS_OFFSET.store(address_offset, Ordering::Release);
        unsafe {
            *MESSAGE_WRITTEN.0.get() = message;
            *MESSAGE_UNWRITTEN.0.get() = message_unwritten;
        }
        READY.store(true, Ordering::Release);
        // Preserve the original handler, including for skipped/unknown faults.
        // This module itself never claims recovery; absent an original handler,
        // Windows retains its default fatal/WER handling.
        let previous = unsafe { SetUnhandledExceptionFilter(Some(unhandled)) };
        PREVIOUS_FILTER.store(
            previous
                .filter(|filter| *filter as usize != unhandled as usize)
                .map_or(0, |filter| filter as usize),
            Ordering::Release,
        );
        log_result
    }

    fn append_wide(
        destination: &mut [u16; MESSAGE_CAPACITY],
        position: &mut usize,
        units: impl IntoIterator<Item = u16>,
    ) {
        for unit in units {
            if *position >= MESSAGE_CAPACITY - 1 {
                break;
            }
            destination[*position] = unit;
            *position += 1;
        }
    }

    fn supported_fatal(code: u32) -> bool {
        matches!(
            code,
            ACCESS_VIOLATION
                | ILLEGAL_INSTRUCTION
                | INTEGER_DIVIDE_BY_ZERO
                | INTEGER_OVERFLOW
                | PRIVILEGED_INSTRUCTION
        )
    }

    unsafe fn delegate(pointers: *mut ExceptionPointers) -> i32 {
        let previous = PREVIOUS_FILTER.load(Ordering::Acquire);
        if previous == 0
            || previous == unhandled as usize
            || DELEGATING
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return CONTINUE_SEARCH;
        }
        // Only a function pointer returned by SetUnhandledExceptionFilter is
        // stored here. The guard also avoids recursion if that handler faults.
        let filter: Filter = unsafe { std::mem::transmute(previous) };
        let result = unsafe { filter(pointers) };
        DELEGATING.store(false, Ordering::Release);
        result
    }

    unsafe extern "system" fn unhandled(pointers: *mut ExceptionPointers) -> i32 {
        // Windows supplies these records. Do not inspect the fault stack or
        // arbitrary exception parameters; only fixed exception-record fields.
        if !READY.load(Ordering::Acquire) || pointers.is_null() {
            return unsafe { delegate(pointers) };
        }
        let record = unsafe { (*pointers).record };
        if record.is_null() {
            return unsafe { delegate(pointers) };
        }
        let code = unsafe { (*record).code };
        // This excludes stack overflow, heap corruption, fast-fail, breakpoint,
        // Rust/C++ language exceptions and all unrecognized exception codes.
        if !supported_fatal(code)
            || ENTERED
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return unsafe { delegate(pointers) };
        }
        let address = unsafe { (*record).address } as usize;
        let flags = unsafe { (*record).flags };
        let thread = unsafe { GetCurrentThreadId() };
        let mut time = SystemTime {
            year: 0,
            month: 0,
            day_of_week: 0,
            day: 0,
            hour: 0,
            minute: 0,
            second: 0,
            milliseconds: 0,
        };
        unsafe { GetSystemTime(&mut time) };
        let line = encode_exception(
            code,
            flags,
            address,
            thread,
            IMAGE_BASE.load(Ordering::Acquire),
            unsafe { GetCurrentProcessId() },
            time,
        );
        let handle = APPEND_HANDLE.load(Ordering::Acquire);
        let mut recorded = false;
        if handle != 0 && handle != -1 {
            let mut written = 0;
            recorded = unsafe {
                WriteFile(
                    handle as *mut c_void,
                    line.bytes.as_ptr().cast(),
                    line.length as u32,
                    &mut written,
                    ptr::null_mut(),
                )
            } != 0
                && written as usize == line.length;
        }
        if ALLOW_DIALOG.load(Ordering::Acquire) && crate::crash_dialog::claim_failure_dialog() {
            // MessageBoxW itself is best-effort. No GUI runtime dispatch,
            // heap-backed formatting, symbols or application locks are used.
            let message = unsafe {
                &mut *if recorded {
                    MESSAGE_WRITTEN.0.get()
                } else {
                    MESSAGE_UNWRITTEN.0.get()
                }
            };
            write_wide_hex(message, CODE_OFFSET.load(Ordering::Acquire), code as u64, 8);
            write_wide_hex(
                message,
                ADDRESS_OFFSET.load(Ordering::Acquire),
                address as u64,
                16,
            );
            unsafe {
                MessageBoxW(
                    ptr::null_mut(),
                    message.as_ptr(),
                    TITLE.as_ptr(),
                    0x10 | 0x2000 | 0x10000,
                )
            };
        }
        unsafe { delegate(pointers) }
    }

    fn write_wide_hex(destination: &mut [u16], offset: usize, value: u64, digits: usize) {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        for index in 0..digits {
            if let Some(unit) = destination.get_mut(offset.saturating_add(index)) {
                *unit = HEX[((value >> ((digits - index - 1) * 4)) & 15) as usize] as u16;
            }
        }
    }

    struct Line {
        bytes: [u8; 768],
        length: usize,
    }

    impl Line {
        fn new() -> Self {
            Self {
                bytes: [0; 768],
                length: 0,
            }
        }
        fn push(&mut self, byte: u8) {
            if let Some(slot) = self.bytes.get_mut(self.length) {
                *slot = byte;
                self.length += 1;
            }
        }
        fn text(&mut self, text: &[u8]) {
            for &byte in text {
                self.push(byte);
            }
        }
        fn hex(&mut self, value: u64, digits: usize) {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            for index in 0..digits {
                self.push(HEX[((value >> ((digits - index - 1) * 4)) & 15) as usize]);
            }
        }
        fn decimal(&mut self, mut value: u32, width: usize) {
            let mut digits = [b'0'; 10];
            let mut start = digits.len();
            loop {
                start -= 1;
                digits[start] = b'0' + (value % 10) as u8;
                value /= 10;
                if value == 0 {
                    break;
                }
            }
            let count = digits.len() - start;
            for _ in count..width {
                self.push(b'0');
            }
            self.text(&digits[start..]);
        }
    }

    fn encode_exception(
        code: u32,
        flags: u32,
        address: usize,
        thread: u32,
        image_base: usize,
        pid: u32,
        time: SystemTime,
    ) -> Line {
        let mut line = Line::new();
        line.text(b"{\"schemaVersion\":1,\"timeUtc\":\"");
        line.decimal(time.year.into(), 4);
        line.push(b'-');
        line.decimal(time.month.into(), 2);
        line.push(b'-');
        line.decimal(time.day.into(), 2);
        line.push(b'T');
        line.decimal(time.hour.into(), 2);
        line.push(b':');
        line.decimal(time.minute.into(), 2);
        line.push(b':');
        line.decimal(time.second.into(), 2);
        line.push(b'.');
        line.decimal(time.milliseconds.into(), 3);
        line.text(b"Z\",\"pid\":");
        line.decimal(pid, 0);
        line.text(b",\"event\":\"native_exception\",\"phase\":\"native.unhandled\",\"details\":{\"exceptionCode\":\"0x");
        line.hex(code.into(), 8);
        line.text(b"\",\"exceptionFlags\":\"0x");
        line.hex(flags.into(), 8);
        line.text(b"\",\"exceptionAddress\":\"0x");
        line.hex(address as u64, 16);
        line.text(b"\",\"threadId\":");
        line.decimal(thread, 0);
        line.text(b",\"imageBase\":\"0x");
        line.hex(image_base as u64, 16);
        line.text(b"\",\"faultStackCaptured\":false}}\n");
        line
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{
            os::windows::process::CommandExt,
            process::{Command, Stdio},
            time::{Duration, Instant, SystemTime as Clock, UNIX_EPOCH},
        };

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn RaiseException(code: u32, flags: u32, count: u32, arguments: *const usize);
            fn SetErrorMode(mode: u32) -> u32;
        }

        unsafe extern "system" fn previous_filter(_: *mut ExceptionPointers) -> i32 {
            let bytes = b"{\"event\":\"previous_filter\"}\n";
            let mut written = 0;
            unsafe {
                WriteFile(
                    APPEND_HANDLE.load(Ordering::Acquire) as *mut c_void,
                    bytes.as_ptr().cast(),
                    bytes.len() as u32,
                    &mut written,
                    ptr::null_mut(),
                );
            }
            CONTINUE_SEARCH
        }

        #[test]
        fn native_record_is_bounded_json_with_fault_identity_without_a_claimed_stack() {
            let line = encode_exception(
                ACCESS_VIOLATION,
                0,
                0x1234,
                42,
                0x1000,
                u32::MAX,
                SystemTime {
                    year: 2026,
                    month: 10,
                    day_of_week: 5,
                    day: 16,
                    hour: 12,
                    minute: 34,
                    second: 56,
                    milliseconds: 7,
                },
            );
            let value: serde_json::Value =
                serde_json::from_slice(&line.bytes[..line.length]).unwrap();
            assert_eq!(value["timeUtc"], "2026-10-16T12:34:56.007Z");
            assert_eq!(value["details"]["exceptionCode"], "0xC0000005");
            assert_eq!(value["details"]["exceptionAddress"], "0x0000000000001234");
            assert_eq!(value["details"]["imageBase"], "0x0000000000001000");
            assert_eq!(value["details"]["faultStackCaptured"], false);
            assert_eq!(value["pid"], u32::MAX);
            assert_eq!(line.bytes[line.length - 1], b'\n');
        }

        #[test]
        fn dangerous_and_language_exceptions_are_never_treated_as_supported_faults() {
            for code in [
                0xc000_00fd,
                0xc000_0374,
                0xc000_0409,
                0x8000_0003,
                0xe06d_7363,
                0xeac1_0001,
            ] {
                assert!(!supported_fatal(code));
            }
            assert!(supported_fatal(ACCESS_VIOLATION));
            assert_eq!(unsafe { unhandled(ptr::null_mut()) }, CONTINUE_SEARCH);
        }

        /// Run this test through the parent below, never against a product PID.
        #[test]
        #[ignore = "Intentional fatal exception in a fresh isolated child; parent test invokes it"]
        fn native_crash_child() {
            let Some(path) = std::env::var_os("CLIPHOST_NATIVE_CRASH_TEST_LOG") else {
                return;
            };
            let mode = std::env::var("CLIPHOST_NATIVE_CRASH_TEST_MODE").unwrap();
            unsafe { SetErrorMode(0x0001 | 0x0002) };
            let path = std::path::PathBuf::from(path);
            if mode == "previous" || mode == "previous-unknown" {
                unsafe { SetUnhandledExceptionFilter(Some(previous_filter)) };
            }
            if mode == "gate-claimed" || mode == "unavailable" {
                assert!(crate::crash_dialog::claim_failure_dialog());
            }
            if mode == "unavailable" {
                assert!(install(None, true).is_err());
                // Log failure still registered the callback and its prepared
                // fallback. Re-register the same filter to inspect the former.
                let previous = unsafe { SetUnhandledExceptionFilter(Some(unhandled)) };
                assert_eq!(
                    previous.map(|filter| filter as usize),
                    Some(unhandled as usize)
                );
                assert!(READY.load(Ordering::Acquire));
                assert_eq!(APPEND_HANDLE.load(Ordering::Acquire), 0);
            } else {
                install(Some(&path), mode == "gate-claimed").unwrap();
            }
            if mode == "av" {
                let address = std::hint::black_box(1_usize) as *mut u8;
                unsafe { std::ptr::write_volatile(address, 7) };
            } else if mode == "previous-unknown" {
                unsafe { RaiseException(0xeac1_0001, 1, 0, ptr::null()) };
            } else {
                unsafe { RaiseException(INTEGER_DIVIDE_BY_ZERO, 1, 0, ptr::null()) };
            }
            std::process::exit(99);
        }

        #[test]
        fn real_isolated_native_faults_persist_identity_and_keep_the_shared_dialog_gate() {
            let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../..")
                .join(".tmp")
                .join(format!(
                    "native-crash-{}-{}",
                    std::process::id(),
                    Clock::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
                ));
            std::fs::create_dir_all(&root).unwrap();
            for mode in [
                "av",
                "raised",
                "gate-claimed",
                "unavailable",
                "previous",
                "previous-unknown",
            ] {
                let path = root.join(format!("{mode}.jsonl"));
                std::fs::write(&path, b"{\"event\":\"fixture_start\"}\n").unwrap();
                let mut child = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "native_crash::windows_impl::tests::native_crash_child",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("CLIPHOST_NATIVE_CRASH_TEST_LOG", &path)
                    .env("CLIPHOST_NATIVE_CRASH_TEST_MODE", mode)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .creation_flags(0x0800_0000)
                    .spawn()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(15);
                let status = loop {
                    if let Some(status) = child.try_wait().unwrap() {
                        break status;
                    }
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!(
                            "isolated native fault did not terminate (mode={mode}); no product process was touched"
                        );
                    }
                    std::thread::sleep(Duration::from_millis(25));
                };
                assert!(!status.success());
                assert_eq!(
                    status.code().map(|code| code as u32),
                    Some(if mode == "av" {
                        ACCESS_VIOLATION
                    } else if mode == "previous-unknown" {
                        0xeac1_0001
                    } else {
                        INTEGER_DIVIDE_BY_ZERO
                    })
                );
                let entries = std::fs::read_to_string(&path).unwrap();
                let entries: Vec<serde_json::Value> = entries
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                if mode == "unavailable" {
                    assert_eq!(entries.len(), 1);
                    assert_eq!(entries[0]["event"], "fixture_start");
                    continue;
                }
                if mode == "previous-unknown" {
                    assert_eq!(entries.len(), 2);
                    assert_eq!(entries[1]["event"], "previous_filter");
                    continue;
                }
                assert_eq!(entries.len(), if mode == "previous" { 3 } else { 2 });
                if mode == "previous" {
                    assert_eq!(entries[2]["event"], "previous_filter");
                }
                assert_eq!(entries[0]["event"], "fixture_start");
                assert_eq!(entries[1]["event"], "native_exception");
                assert_eq!(entries[1]["pid"], child.id());
                assert_eq!(
                    entries[1]["details"]["exceptionCode"],
                    if mode == "av" {
                        "0xC0000005"
                    } else {
                        "0xC0000094"
                    }
                );
                if mode == "av" {
                    assert_eq!(entries[1]["details"]["exceptionFlags"], "0x00000000");
                }
                assert_eq!(entries[1]["details"]["faultStackCaptured"], false);
            }
        }
    }
}
