//! C# `ClipHostProcessRestart` + `ClipHostUnelevatedLaunch`.

use std::path::{Path, PathBuf};
use std::process::Command;
use parking_lot::Mutex;

static REPLACEMENT_LAUNCHED: Mutex<bool> = Mutex::new(false);

/// C# `ClipHostUnelevatedLaunch.DemoteAttemptedArg` (used by current-exe unelevated relaunch).
pub const DEMOTE_ATTEMPTED_ARG: &str = "--cliphost-demote-attempted";

/// C# `LaunchNewInstanceAndExit` spawn half — caller still exits the current process.
pub fn launch_new_instance(prefer_medium_integrity: bool) -> Result<(), String> {
    launch_once(&REPLACEMENT_LAUNCHED, || {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let mut args = crate::restart_handoff::replacement_arguments()?;
        if prefer_medium_integrity { crate::restart_environment::append_marker(&mut args); }
        let command_line = args.iter().map(|arg| quote_for_process_argument(arg)).collect::<Vec<_>>().join(" ");
        launch_for_mode(prefer_medium_integrity,
            || try_launch_executable_unelevated(&exe, &command_line),
            || spawn_replacement(&exe, &args))
    })
}

fn launch_for_mode(
    prefer_medium: bool,
    medium: impl FnOnce() -> bool,
    normal: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if prefer_medium {
        return if medium() { Ok(()) }
            else { Err("Cannot launch an ordinary-permission ClipHost through the desktop Explorer; current instance retained.".into()) };
    }
    normal()
}

fn launch_once(state: &Mutex<bool>, launch: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    let mut launched = state.lock();
    if !*launched {
        // Wait for the launch result before treating a concurrent request as
        // successful. A failed launch must neither schedule exit nor block retry.
        launch()?;
        *launched = true;
    }
    Ok(())
}

/// Run before single-instance setup, elevation and any database/network startup.
pub fn wait_for_predecessor() -> Result<(), String> {
    restore_relaunch_environment()?;
    crate::restart_handoff::wait_for_predecessor()
}

/// Restore ClipHost-only overrides before resolving paths, elevation or startup.
/// Return argv without the internal marker for standalone native probe callers.
pub fn restore_relaunch_environment() -> Result<Vec<String>, String> {
    crate::restart_environment::restore(std::env::args().collect())
}

pub fn command_line_has_demote_attempted() -> bool {
    std::env::args().any(|arg| arg.eq_ignore_ascii_case(DEMOTE_ATTEMPTED_ARG))
}

pub fn build_relaunch_arguments_including_demote_marker() -> String {
    build_relaunch_arguments_including_demote_marker_from(std::env::args().skip(1))
}

pub fn build_relaunch_arguments_including_demote_marker_from<I, S>(args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut parts = Vec::new();
    for arg in args {
        if arg.as_ref().eq_ignore_ascii_case(DEMOTE_ATTEMPTED_ARG)
            || arg.as_ref().starts_with(crate::restart_environment::ARG_PREFIX) {
            continue;
        }
        parts.push(quote_for_process_argument(arg.as_ref()));
    }
    // Quote the marker separately; its base64 payload contains no whitespace.
    let mut marker = Vec::new();
    crate::restart_environment::append_marker(&mut marker);
    parts.extend(marker);
    parts.push(DEMOTE_ATTEMPTED_ARG.to_string());
    parts.join(" ")
}

/// C# `TryLaunchCurrentExecutableUnelevated` (adds `--cliphost-demote-attempted`).
pub fn try_launch_current_executable_unelevated() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    if !exe.is_file() {
        return false;
    }
    try_launch_executable_unelevated(&exe, &build_relaunch_arguments_including_demote_marker())
}

pub fn notify_spawn_failed_if_possible() {
    #[cfg(windows)]
    {
        use windows::core::{w, HSTRING, PCWSTR};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONWARNING, MB_OK};

        let text = HSTRING::from(
            "未能自动启动新实例，请手动重新打开程序。数据文件夹设置已保存。",
        );
        unsafe {
            let _ = MessageBoxW(
                HWND::default(),
                PCWSTR(text.as_ptr()),
                w!("ClipHost"),
                MB_OK | MB_ICONWARNING,
            );
        }
    }
}

/// Windows argument quoting for relaunch, ShellExecute and scheduled tasks.
/// Backslashes before a quote (including the closing quote) must be doubled.
pub fn quote_for_process_argument(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".into();
    }
    let needs_quotes = arg.chars().any(|ch| ch == ' ' || ch == '\t' || ch == '"');
    if !needs_quotes {
        return arg.to_string();
    }
    let mut quoted = String::from("\"");
    let mut slashes = 0;
    for character in arg.chars() {
        if character == '\\' {
            slashes += 1;
            continue;
        }
        quoted.extend(std::iter::repeat_n('\\', if character == '"' { slashes * 2 + 1 } else { slashes }));
        quoted.push(character);
        slashes = 0;
    }
    quoted.extend(std::iter::repeat_n('\\', slashes * 2));
    quoted.push('"');
    quoted
}

pub fn try_launch_executable_unelevated(exe: &Path, arguments: &str) -> bool {
    if !exe.is_file() || exe.as_os_str().to_string_lossy().encode_utf16().count()
        + arguments.encode_utf16().count() + 4 >= 32_767 {
        return false;
    }
    let work_dir = exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(system_directory);
    // A broker failure must not be hidden by an unverified launch, and every
    // restart/demotion marker must reach the replacement unchanged.
    try_shell_application_execute(exe, arguments, &work_dir)
}

fn spawn_replacement(exe: &Path, args: &[String]) -> Result<(), String> {
    let mut command = Command::new(exe);
    command.args(args)
        .current_dir(exe.parent().unwrap_or_else(|| Path::new(".")))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW; no shell or console flash.
    }
    command.spawn().map(|_| ()).map_err(|error| format!("Cannot launch replacement: {error}"))
}

fn try_shell_application_execute(file: &Path, arguments: &str, working_directory: &Path) -> bool {
    #[cfg(windows)]
    {
        shell_application_execute_windows(file, arguments, working_directory)
    }
    #[cfg(not(windows))]
    {
        let _ = (file, arguments, working_directory);
        false
    }
}

#[cfg(windows)]
fn shell_application_execute_windows(file: &Path, arguments: &str, working_directory: &Path) -> bool {
    use windows_core::{BSTR, VARIANT};
    with_shell_application(|shell| unsafe {
        let file_b = BSTR::from(file.to_string_lossy().as_ref());
        let args = VARIANT::from(BSTR::from(arguments));
        let dir = VARIANT::from(BSTR::from(working_directory.to_string_lossy().as_ref()));
        let op = VARIANT::from(BSTR::from(""));
        let show = VARIANT::from(1i32);
        shell.ShellExecute(&file_b, &args, &dir, &op, &show)
    })
}

#[cfg(windows)]
fn with_shell_application(
    execute: impl FnOnce(&windows::Win32::UI::Shell::IShellDispatch2) -> windows::core::Result<()>,
) -> bool {
    unsafe {
        // Declare first so the Shell interface is released before COM teardown.
        let Ok(_apartment) = crate::explorer::ShellApartment::initialize() else { return false; };
        let shell = match crate::restart_shell::desktop_dispatch() {
            Ok(shell) => shell,
            Err(error) => {
                tracing::warn!(%error, "Desktop Shell restart unavailable");
                #[cfg(test)]
                eprintln!("Desktop Shell restart unavailable: {error:?}");
                return false;
            }
        };
        execute(&shell).is_ok()
    }
}

fn windows_dir() -> PathBuf {
    std::env::var_os("WINDIR")
        .or_else(|| std::env::var_os("SystemRoot"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

fn system_directory() -> PathBuf {
    windows_dir().join("System32")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn restart_shell_balances_com_after_success_and_failure() {
        std::thread::spawn(|| unsafe {
            use windows::Win32::Foundation::E_FAIL;
            use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
            for fail in [false, true] {
                // Borrow the actual desktop automation object; process dispatch
                // is replaced, so this test cannot launch a program or a dialog.
                let result = with_shell_application(|_| if fail { Err(E_FAIL.into()) } else { Ok(()) });
                assert_eq!(result, !fail);
                let initialized = CoInitializeEx(None, COINIT_MULTITHREADED);
                if initialized.is_ok() { CoUninitialize(); }
                initialized.ok().expect("restart shell leaked STA initialization");
            }
        }).join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn restart_shell_preserves_existing_apartment() {
        std::thread::spawn(|| unsafe {
            use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
            use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED};
            for mode in [COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED] {
                CoInitializeEx(None, mode).ok().unwrap();
                assert!(with_shell_application(|_| Ok(())));
                let opposite = if mode == COINIT_APARTMENTTHREADED { COINIT_MULTITHREADED } else { COINIT_APARTMENTTHREADED };
                assert_eq!(CoInitializeEx(None, opposite), RPC_E_CHANGED_MODE);
                CoUninitialize();
                let initialized = CoInitializeEx(None, opposite);
                if initialized.is_ok() { CoUninitialize(); }
                initialized.ok().expect("restart shell changed the caller's COM lifetime");
            }
        }).join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn restart_arguments_roundtrip_through_windows_parser() {
        use windows::core::HSTRING;
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::UI::Shell::CommandLineToArgvW;
        let arguments = ["", "普通参数😀", "带 空格", "C:\\space folder\\", "a\\\"b", "a\\\\\"b", "a\t\\", "--cliphost-restart-parent=42:123"];
        let command = format!("fixture.exe {}", arguments.iter().map(|arg| quote_for_process_argument(arg)).collect::<Vec<_>>().join(" "));
        unsafe {
            let mut count = 0;
            let raw = CommandLineToArgvW(&HSTRING::from(command), &mut count);
            assert!(!raw.is_null());
            let parsed: Vec<String> = std::slice::from_raw_parts(raw, count as usize)
                .iter().map(|value| value.to_string().unwrap()).collect();
            LocalFree(HLOCAL(raw.cast()));
            assert_eq!(&parsed[1..], arguments);
        }
    }

    #[test]
    fn failed_medium_restart_never_launches_an_inherited_token_replacement() {
        let normal_calls = std::cell::Cell::new(0);
        let state = Mutex::new(false);
        let failed = launch_once(&state, || launch_for_mode(true, || false, || {
            normal_calls.set(normal_calls.get() + 1);
            Ok(())
        }));
        assert!(failed.is_err(), "failed demotion must not report a successful restart");
        assert_eq!(normal_calls.get(), 0);
        assert!(!*state.lock(), "failure must allow another attempt");
        assert!(launch_once(&state, || launch_for_mode(true, || true, || panic!("medium success must not fall back"))).is_ok());
        assert!(*state.lock());
    }

    #[test]
    fn normal_restart_preserves_failure_and_does_not_attempt_demotion() {
        assert_eq!(launch_for_mode(false, || panic!("not a demotion request"), || Err("spawn failed".into())).unwrap_err(), "spawn failed");
        assert!(launch_for_mode(false, || panic!("not a demotion request"), || Ok(())).is_ok());
    }

    #[test]
    fn concurrent_restart_requests_create_only_one_replacement() {
        use std::sync::{atomic::{AtomicUsize, Ordering}, Arc, Barrier};
        let state = Arc::new(Mutex::new(false));
        let start = Arc::new(Barrier::new(8));
        let calls = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..8).map(|_| {
            let state = state.clone();
            let start = start.clone();
            let calls = calls.clone();
            std::thread::spawn(move || {
                start.wait();
                launch_once(&state, || { calls.fetch_add(1, Ordering::SeqCst); Ok(()) })
            })
        }).collect();
        for worker in workers { assert!(worker.join().unwrap().is_ok()); }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_restart_request_reports_failure_and_allows_a_concurrent_retry() {
        use std::sync::{atomic::{AtomicUsize, Ordering}, Arc, Barrier};
        let state = Arc::new(Mutex::new(false));
        let start = Arc::new(Barrier::new(8));
        let calls = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..8).map(|_| {
            let state = state.clone();
            let start = start.clone();
            let calls = calls.clone();
            std::thread::spawn(move || {
                start.wait();
                launch_once(&state, || {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 { Err("spawn failed".into()) } else { Ok(()) }
                })
            })
        }).collect();
        let results: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 7);
        assert_eq!(results.iter().filter(|result| result.as_ref().err().map(String::as_str) == Some("spawn failed")).count(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn quote_for_process_argument_preserves_simple_arguments() {
        assert_eq!(quote_for_process_argument(""), "\"\"");
        assert_eq!(quote_for_process_argument("cliphost.exe"), "cliphost.exe");
        assert_eq!(
            quote_for_process_argument(r"C:\Program Files\app.exe"),
            "\"C:\\Program Files\\app.exe\""
        );
        assert_eq!(quote_for_process_argument("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_for_process_argument("a\tb"), "\"a\tb\"");
    }

    #[test]
    fn demote_relaunch_args_skip_existing_marker_and_append_once() {
        let line = build_relaunch_arguments_including_demote_marker_from([
            "clip show",
            DEMOTE_ATTEMPTED_ARG,
            "--cliphost-demote-attempted",
        ]);
        assert!(line.ends_with(DEMOTE_ATTEMPTED_ARG));
        assert_eq!(line.matches(DEMOTE_ATTEMPTED_ARG).count(), 1);
        assert!(line.contains("\"clip show\""));
    }
}
