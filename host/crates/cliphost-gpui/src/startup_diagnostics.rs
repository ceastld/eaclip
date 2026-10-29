//! Startup evidence must survive failures before the selected data root and
//! tracing subscriber exist. This file never opens clipboard or product data.

use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};
use std::{
    backtrace::Backtrace,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    panic,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

const RETAINED_LOGS: usize = 20;
const ROUTINE_LOG_LIMIT: u64 = 2 * 1024 * 1024;
static ACTIVE: OnceLock<Arc<StartupLog>> = OnceLock::new();
static INTERACTIVE: OnceLock<bool> = OnceLock::new();

struct StartupLog {
    file: Mutex<(File, u64)>,
    phase: Mutex<&'static str>,
    path: PathBuf,
}

impl StartupLog {
    // Tests supply their own directory; they never resolve the production one.
    fn create(directory: &Path) -> io::Result<(Arc<Self>, PathBuf)> {
        fs::create_dir_all(directory)?;
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let path = directory.join(format!("startup-{millis}-{}.jsonl", std::process::id()));
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        let log = Arc::new(Self {
            file: Mutex::new((file, 0)),
            phase: Mutex::new("main.enter"),
            path: path.clone(),
        });
        log.record(
            "process_start",
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "renderer": "gpui",
                "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
                "architecture": std::env::consts::ARCH,
                "os": std::env::consts::OS,
                "imageBase": executable_image_base(),
                "nativeAcceptance": cfg!(feature = "native-acceptance"),
            }),
            true,
        )?;
        retain_recent_logs(directory, &path);
        Ok((log, path))
    }

    fn record(&self, event: &str, details: Value, fatal: bool) -> io::Result<()> {
        let phase = *self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bytes = encode_entry(event, phase, details)?;
        let mut writer = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !fatal && writer.1 >= ROUTINE_LOG_LIMIT {
            return Ok(());
        }
        // One escaped JSON object per line, even for multiline GPU errors and
        // panic backtraces. Flush every record before continuing startup.
        writer.0.write_all(&bytes)?;
        writer.0.flush()?;
        writer.1 = writer.1.saturating_add(bytes.len() as u64);
        // Fatal records bypass the routine cap, so a noisy driver cannot hide
        // the eventual failure.
        Ok(())
    }

    fn phase(&self, phase: &'static str) {
        *self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = phase;
        let _ = self.record("phase", Value::Null, false);
    }

    fn completed(&self) -> io::Result<()> {
        // Preserve completion even after repeated driver warnings fill the log.
        self.record("run_completed", json!({ "exitCode": 0 }), true)
    }

    fn record_panic(&self, message: String, backtrace: String) -> io::Result<()> {
        // A panic may originate in this logger while either mutex is held.
        // Never wait for those locks from the panic hook: append through an
        // independent handle when the ordinary writer cannot be borrowed.
        let phase = self
            .phase
            .try_lock()
            .map(|phase| *phase)
            .unwrap_or("diagnostics.lock_busy");
        let bytes = encode_entry(
            "panic",
            phase,
            json!({ "message": message, "backtrace": backtrace, "imageBase": executable_image_base() }),
        )?;
        if let Ok(mut writer) = self.file.try_lock() {
            writer.0.write_all(&bytes).and_then(|_| writer.0.flush())?;
            writer.1 = writer.1.saturating_add(bytes.len() as u64);
        } else {
            let mut file = OpenOptions::new().append(true).open(&self.path)?;
            file.write_all(&bytes).and_then(|_| file.flush())?;
        }
        Ok(())
    }
}

fn encode_entry(event: &str, phase: &str, details: Value) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(&json!({
        "schemaVersion": 1,
        "timeUtc": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "pid": std::process::id(),
        "event": event,
        "phase": phase,
        "details": details,
    }))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn executable_image_base() -> Option<String> {
    #[cfg(windows)]
    {
        use std::{ffi::c_void, ptr};
        #[link(name = "kernel32")]
        extern "system" {
            fn GetModuleHandleW(name: *const u16) -> *mut c_void;
        }
        let module = unsafe { GetModuleHandleW(ptr::null()) };
        (!module.is_null()).then(|| format!("0x{:x}", module as usize))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn default_log_allowed(
    arguments: impl IntoIterator<Item = OsString>,
    native_acceptance: bool,
) -> bool {
    if native_acceptance {
        return false;
    }
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some(
                "--fixture" | "--inspect" | "--stress" | "--snapshot" | "--test-instance"
                | "--headless" | "--build-info-json",
            ) => return false,
            // A search value may itself look like a mode flag. Follow the
            // actual option parser's value consumption without recording it.
            Some(
                "--theme"
                | "--language"
                | "--type"
                | "--query"
                | "--appearance"
                | "--width"
                | "--height"
                | "--data-root"
                | "--api-port"
                | "--exit-after-ms"
                | "--report"
                | "--qa-content-fixture",
            ) => {
                arguments.next();
            }
            _ => {}
        }
    }
    true
}

fn default_directory(local_application_data: Option<OsString>) -> Option<PathBuf> {
    let root = PathBuf::from(local_application_data?);
    root.is_absolute()
        .then(|| root.join("ClipHostRust").join("diagnostics"))
}

fn is_startup_log(name: &str) -> bool {
    name.strip_prefix("startup-")
        .and_then(|name| name.strip_suffix(".jsonl"))
        .and_then(|name| name.split_once('-'))
        .is_some_and(|(time, pid)| {
            !time.is_empty()
                && !pid.is_empty()
                && time.bytes().all(|byte| byte.is_ascii_digit())
                && pid.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn retain_recent_logs(directory: &Path, current: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut logs: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && entry.file_name().to_str().is_some_and(is_startup_log)
        })
        .collect();
    logs.sort_by_key(|entry| entry.file_name());
    let remove = logs.len().saturating_sub(RETAINED_LOGS);
    for entry in logs
        .into_iter()
        .filter(|entry| entry.path() != current)
        .take(remove)
    {
        let _ = fs::remove_file(entry.path());
    }
}

fn install_panic_hook(log: Option<Arc<StartupLog>>, interactive: bool, fallback: Option<PathBuf>) {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let message = info.to_string();
        let backtrace = Backtrace::force_capture();
        let captured = backtrace.status() == std::backtrace::BacktraceStatus::Captured;
        // Full formatting preserves instruction addresses even without an
        // installed PDB. imageBase lets an offline matching PDB undo ASLR.
        let stack = format!("{backtrace:#}");
        let saved = log
            .as_ref()
            .is_some_and(|log| log.record_panic(message.clone(), stack.clone()).is_ok());
        // record_panic has released every logging lock before a hook or modal
        // dialog can run. Capture only once, before invoking the previous hook.
        previous(info);
        crate::crash_dialog::show_panic(
            interactive,
            &message,
            captured.then_some(stack.as_str()),
            log.as_ref()
                .map(|log| log.path.as_path())
                .or(fallback.as_deref()),
            saved,
        );
    }));
}

pub(crate) fn initialize() {
    if INTERACTIVE.get().is_some() {
        return;
    }
    let interactive = default_log_allowed(
        std::env::args_os().skip(1),
        cfg!(feature = "native-acceptance"),
    );
    let _ = INTERACTIVE.set(interactive);
    if !interactive {
        return;
    }
    let directory = default_directory(std::env::var_os("LOCALAPPDATA"));
    if let Some(directory) = &directory {
        match StartupLog::create(directory) {
            Ok((log, _)) => {
                let _ = ACTIVE.set(log);
            }
            Err(error) => eprintln!("ClipHost startup diagnostics could not be opened: {error}"),
        }
    }
    // A read-only/missing diagnostic directory must not disable error dialogs.
    install_panic_hook(ACTIVE.get().cloned(), interactive, directory);
}

pub(crate) fn phase(phase: &'static str) {
    // Private runs have no global startup-log directory; the supervisor
    // captures these fixed labels inside its already owned test directory.
    #[cfg(feature = "native-acceptance")]
    eprintln!("Native acceptance phase: {phase}");
    if let Some(log) = ACTIVE.get() {
        log.phase(phase);
    }
}

/// Initialization-only access for native crash diagnostics. The fault callback
/// receives a prebuilt path and never reads this state itself.
pub(crate) fn active_log_path() -> Option<&'static Path> {
    ACTIVE.get().map(|log| log.path.as_path())
}

pub(crate) fn interactive_failures_allowed() -> bool {
    INTERACTIVE.get().copied().unwrap_or(false)
}

pub(crate) fn warning(level: log::Level, target: &str, message: &str) {
    if let Some(log) = ACTIVE.get() {
        let lifecycle = target == "cliphost_gpui::lifecycle";
        let _ = log.record(
            if lifecycle { "lifecycle" } else { "log" },
            json!({ "level": level.as_str(), "target": target, "message": message }),
            lifecycle,
        );
    }
}

pub(crate) fn run_failed(error: &str) {
    let saved = ACTIVE.get().is_some_and(|log| {
        log.record(
            "run_error",
            json!({ "message": error, "exitCode": 1, "imageBase": executable_image_base() }),
            true,
        )
        .is_ok()
    });
    let fallback = default_directory(std::env::var_os("LOCALAPPDATA"));
    crate::crash_dialog::show_run_failure(
        INTERACTIVE.get().copied().unwrap_or(false),
        error,
        ACTIVE
            .get()
            .map(|log| log.path.as_path())
            .or(fallback.as_deref()),
        saved,
    );
}

pub(crate) fn run_completed() {
    if let Some(log) = ACTIVE.get() {
        let _ = log.completed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn normal_completion_survives_the_routine_log_limit() {
        let root = tempfile::tempdir().unwrap();
        let (log, path) = StartupLog::create(root.path()).unwrap();
        log.file.lock().unwrap().1 = ROUTINE_LOG_LIMIT;
        log.record("log", json!({"message":"routine warning"}), false).unwrap();
        log.completed().unwrap();
        let entries = read(&path);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1]["event"], "run_completed");
        assert_eq!(entries[1]["details"]["exitCode"], 0);
    }

    #[test]
    fn startup_and_multiline_failure_are_flushed_in_the_injected_directory() {
        let root = tempfile::tempdir().unwrap();
        let (log, path) = StartupLog::create(root.path()).unwrap();
        log.phase("gpui.application.create");
        log.record(
            "run_error",
            json!({ "message": "GPU failure\nsecond line", "exitCode": 1 }),
            true,
        )
        .unwrap();
        // Read while the writer is alive: a crash need not run its destructor.
        let entries = read(&path);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["event"], "process_start");
        assert_eq!(entries[0]["details"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            entries[0]["details"]["architecture"],
            std::env::consts::ARCH
        );
        #[cfg(windows)]
        assert!(entries[0]["details"]["imageBase"]
            .as_str()
            .unwrap()
            .starts_with("0x"));
        assert_eq!(entries[2]["phase"], "gpui.application.create");
        assert_eq!(entries[2]["details"]["message"], "GPU failure\nsecond line");
        assert_eq!(entries[2]["details"]["exitCode"], 1);
        assert_eq!(entries[2]["schemaVersion"], 1);
    }

    #[test]
    fn fatal_records_survive_routine_log_limit() {
        let root = tempfile::tempdir().unwrap();
        let (log, path) = StartupLog::create(root.path()).unwrap();
        log.file.lock().unwrap().1 = ROUTINE_LOG_LIMIT;
        log.record("log", json!({ "message": "ignored" }), false)
            .unwrap();
        log.record("panic", json!({ "message": "preserved" }), true)
            .unwrap();
        let entries = read(&path);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1]["event"], "panic");
    }

    #[test]
    fn panic_record_never_waits_for_its_own_logging_locks() {
        let root = tempfile::tempdir().unwrap();
        let (log, path) = StartupLog::create(root.path()).unwrap();
        let _writer = log.file.lock().unwrap();
        let _phase = log.phase.lock().unwrap();
        log.record_panic("locked writer panic".into(), "backtrace fixture".into())
            .unwrap();
        let entries = read(&path);
        assert_eq!(entries[1]["event"], "panic");
        assert_eq!(entries[1]["phase"], "diagnostics.lock_busy");
        assert_eq!(entries[1]["details"]["message"], "locked writer panic");
    }

    #[test]
    fn test_and_metadata_modes_never_choose_production_diagnostics() {
        let allowed = |arguments: &[&str], native_acceptance| {
            default_log_allowed(arguments.iter().map(OsString::from), native_acceptance)
        };
        for mode in [
            "--fixture",
            "--inspect",
            "--stress",
            "--snapshot",
            "--test-instance",
            "--headless",
            "--build-info-json",
        ] {
            assert!(!allowed(&[mode], false), "{mode}");
            assert!(!allowed(&["--query", "sample", mode], false), "{mode}");
        }
        assert!(!allowed(&[], true));
        assert!(allowed(&[], false));
        assert!(allowed(&["--background"], false));
        assert!(allowed(&["--query", "--fixture"], false));
        assert!(allowed(
            &["--data-root", "absolute-product-directory"],
            false
        ));
    }

    #[test]
    fn retention_only_removes_older_matching_regular_files() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..25 {
            fs::write(
                root.path()
                    .join(format!("startup-100000000{index:04}-1.jsonl")),
                "old",
            )
            .unwrap();
        }
        let unrelated = root.path().join("startup-user-notes.jsonl");
        fs::write(&unrelated, "keep").unwrap();
        let directory = root.path().join("startup-1000000000000-2.jsonl");
        fs::create_dir(&directory).unwrap();
        let (_log, current) = StartupLog::create(root.path()).unwrap();
        assert!(current.is_file() && unrelated.is_file() && directory.is_dir());
        assert_eq!(
            fs::read_dir(root.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().unwrap().is_file()
                    && entry.file_name().to_str().is_some_and(is_startup_log))
                .count(),
            RETAINED_LOGS
        );
    }

    #[test]
    fn missing_relative_or_unwritable_diagnostic_directory_does_not_choose_user_data() {
        assert!(default_directory(None).is_none());
        assert!(default_directory(Some(OsString::from("relative"))).is_none());
        let root = tempfile::tempdir().unwrap();
        let blocked = root.path().join("file");
        fs::write(&blocked, "keep").unwrap();
        assert!(StartupLog::create(&blocked).is_err());
        assert_eq!(fs::read_to_string(blocked).unwrap(), "keep");
    }

    #[test]
    fn panic_hook_child() {
        let Some(root) = std::env::var_os("CLIPHOST_STARTUP_DIAGNOSTIC_TEST_DIR") else {
            return;
        };
        if std::env::var_os("CLIPHOST_STARTUP_DIAGNOSTIC_TEST_NO_LOG").is_some() {
            install_panic_hook(None, false, None);
            panic!("startup diagnostic panic without writable log");
        }
        let (log, _) = StartupLog::create(Path::new(&root)).unwrap();
        log.phase("panic.fixture");
        install_panic_hook(Some(log), false, None);
        panic!("startup diagnostic panic fixture");
    }

    #[test]
    fn panic_hook_preserves_phase_message_and_backtrace_in_an_isolated_child() {
        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "startup_diagnostics::tests::panic_hook_child",
                "--nocapture",
            ])
            .env("CLIPHOST_STARTUP_DIAGNOSTIC_TEST_DIR", root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let path = fs::read_dir(root.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let entries = read(&path);
        let panic = entries
            .iter()
            .find(|entry| entry["event"] == "panic")
            .unwrap();
        assert_eq!(panic["phase"], "panic.fixture");
        assert!(panic["details"]["message"]
            .as_str()
            .unwrap()
            .contains("startup diagnostic panic fixture"));
        assert!(!panic["details"]["backtrace"].as_str().unwrap().is_empty());
        #[cfg(windows)]
        assert!(panic["details"]["backtrace"]
            .as_str()
            .unwrap()
            .contains("0x"));
    }

    #[test]
    fn panic_hook_still_runs_without_a_writable_log_and_keeps_tests_noninteractive() {
        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "startup_diagnostics::tests::panic_hook_child",
                "--nocapture",
            ])
            .env("CLIPHOST_STARTUP_DIAGNOSTIC_TEST_DIR", root.path())
            .env("CLIPHOST_STARTUP_DIAGNOSTIC_TEST_NO_LOG", "1")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("startup diagnostic panic without writable log"));
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }
}
