//! Explicit native Shell launch probe. No ClipHost, UI, clipboard or UAC is started.
#![cfg_attr(windows, windows_subsystem = "windows")]

use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

fn owned_root(raw: &str) -> Result<PathBuf, String> {
    let root = PathBuf::from(raw)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let clip_repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let temporary = clip_repo_root
        .join(".tmp")
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if root == temporary || !root.starts_with(temporary) {
        return Err("Probe output must be a dedicated workspace .tmp directory".into());
    }
    Ok(root)
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).map_err(|e| e.to_string())
}

#[cfg(windows)]
fn native_identity() -> Result<Value, String> {
    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{GetTokenInformation, GetSidSubAuthorityCount, GetSidSubAuthority,
            TokenIntegrityLevel, TOKEN_MANDATORY_LABEL, TOKEN_QUERY},
        System::{
            Threading::{GetCurrentProcess, OpenProcessToken},
            Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW,
                PROCESSENTRY32W, TH32CS_SNAPPROCESS},
        },
        UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId},
    };
    struct Owned(HANDLE);
    impl Drop for Owned { fn drop(&mut self) { unsafe { let _ = CloseHandle(self.0); } } }
    unsafe {
        let mut raw = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw).map_err(|e| e.to_string())?;
        let token = Owned(raw);
        let mut size = 0;
        let _ = GetTokenInformation(token.0, TokenIntegrityLevel, None, 0, &mut size);
        if size == 0 { return Err("Cannot read child integrity".into()); }
        let mut data = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        GetTokenInformation(token.0, TokenIntegrityLevel, Some(data.as_mut_ptr().cast()), size, &mut size)
            .map_err(|e| e.to_string())?;
        let label = &*(data.as_ptr().cast::<TOKEN_MANDATORY_LABEL>());
        let count = *GetSidSubAuthorityCount(label.Label.Sid);
        if count == 0 { return Err("Child integrity SID is empty".into()); }
        let integrity = *GetSidSubAuthority(label.Label.Sid, u32::from(count - 1));
        let snapshot = Owned(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).map_err(|e| e.to_string())?);
        let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        Process32FirstW(snapshot.0, &mut entry).map_err(|e| e.to_string())?;
        loop {
            if entry.th32ProcessID == std::process::id() { break; }
            Process32NextW(snapshot.0, &mut entry).map_err(|e| e.to_string())?;
        }
        let mut desktop_pid = 0;
        GetWindowThreadProcessId(GetShellWindow(), Some(&mut desktop_pid));
        Ok(json!({"integrityRid": integrity, "parentPid": entry.th32ParentProcessID, "desktopPid": desktop_pid}))
    }
}

#[cfg(not(windows))]
fn native_identity() -> Result<Value, String> { Err("Windows required".into()) }

fn receiver(root: &Path, nonce: &str, arguments: &[String]) -> Result<(), String> {
    let temporary = root.join("child-ready.tmp");
    write_json(
        &temporary,
        &json!({
            "pid": std::process::id(), "nonce": nonce, "arguments": arguments,
            "cwd": std::env::current_dir().map_err(|e| e.to_string())?,
            "environment": std::env::var("CLIPHOST_TAURI_RESTART_PROBE_VALUE").ok(),
            "elevated": cliphost_core::elevation::is_current_process_elevated(),
            "nativeIdentity": native_identity()?,
            "dataRoot": std::env::var("BETTERCLIP_TAURI_DATA_ROOT").ok(),
            "apiPort": std::env::var("INTELLITOOLS_CLIPHOST_TAURI_PORT").ok(),
            "unrelatedEnvironment": std::env::var("CLIPHOST_UNRELATED_PROBE_VALUE").ok(),
        }),
    )?;
    fs::rename(temporary, root.join("child-ready.json")).map_err(|e| e.to_string())?;
    // Keep the process alive until the launcher owns an exit-code handle. A
    // failed launcher cannot leave a permanent helper process behind.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !root.join("allow-child-exit").exists() {
        if Instant::now() >= deadline {
            return Err("Launcher did not release child".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

#[cfg(windows)]
fn launch(root: &Path) -> Result<Value, String> {
    use cliphost_core::process_restart::{
        build_relaunch_arguments_including_demote_marker_from, try_launch_executable_unelevated,
    };
    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        System::Threading::{
            GetExitCodeProcess, OpenProcess, WaitForSingleObject,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        },
    };
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    if root.join("child-ready.json").exists() || root.join("allow-child-exit").exists() {
        return Err("Choose fresh probe output".into());
    }
    let expected_environment =
        std::env::var("CLIPHOST_TAURI_RESTART_PROBE_VALUE").map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let nonce = uuid::Uuid::new_v4().to_string();
    let expected = [
        "",
        "中文😀",
        "C:\\space folder\\",
        "a\\\"b",
        "a\\\\\"b",
        "a\t\\",
        "--cliphost-restart-parent=42:123",
        "--cliphost-demote-attempted",
    ];
    let arguments = build_relaunch_arguments_including_demote_marker_from([
        "--child",
        root.to_str().ok_or("Invalid probe path")?,
        &nonce,
    ]
    .into_iter()
    .chain(expected));
    if !try_launch_executable_unelevated(&exe, &arguments) {
        return Err("Shell launch rejected".into());
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let received: Value = loop {
        if let Ok(bytes) = fs::read(root.join("child-ready.json")) {
            break serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        }
        if Instant::now() >= deadline {
            return Err("Child readiness timed out".into());
        }
        thread::sleep(Duration::from_millis(50));
    };
    let pid = received["pid"].as_u64().ok_or("Child PID missing")? as u32;
    let handle = Handle(
        unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }
        .map_err(|e| e.to_string())?,
    );
    fs::write(root.join("allow-child-exit"), b"exit").map_err(|e| e.to_string())?;
    if unsafe { WaitForSingleObject(handle.0, 15_000) } != WAIT_OBJECT_0 {
        return Err("Child did not exit".into());
    }
    let mut exit_code = 1;
    unsafe { GetExitCodeProcess(handle.0, &mut exit_code) }.map_err(|e| e.to_string())?;
    let cwd = PathBuf::from(received["cwd"].as_str().ok_or("Child cwd missing")?)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let identity = &received["nativeIdentity"];
    let checks = json!({
        "childMedium": identity["integrityRid"].as_u64().is_some_and(|rid| (0x2000..0x3000).contains(&rid)) && received["elevated"] == false,
        "childUsesDesktopParent": identity["desktopPid"].as_u64().is_some_and(|pid| pid != 0) && identity["parentPid"] == identity["desktopPid"],
        "dataRootPreserved": received["dataRoot"] == std::env::var("BETTERCLIP_TAURI_DATA_ROOT").unwrap(),
        "apiPortPreserved": received["apiPort"] == std::env::var("INTELLITOOLS_CLIPHOST_TAURI_PORT").unwrap(),
        "unrelatedEnvironmentNotForwarded": received["unrelatedEnvironment"].is_null() && std::env::var("CLIPHOST_UNRELATED_PROBE_VALUE").is_ok(),
        "nonce": received["nonce"] == nonce,
        "arguments": received["arguments"] == json!(expected),
        "environment": received["environment"] == expected_environment,
        "workingDirectory": cwd == exe.parent().unwrap().canonicalize().map_err(|e| e.to_string())?,
        "childExitedZero": exit_code == 0,
    });
    let ok = checks.as_object().unwrap().values().all(|v| v == true);
    Ok(
        json!({"ok": ok, "checks": checks, "received": received, "childExitCode": exit_code,
        "launcherElevated": cliphost_core::elevation::is_current_process_elevated(),
        "launcherIdentity": native_identity()?,
        "note": "This is a Shell launch/argument/environment probe. Medium-to-medium launch is not evidence of admin demotion."}),
    )
}

fn main() {
    let args = cliphost_core::process_restart::restore_relaunch_environment()
        .expect("valid ClipHost restart environment");
    let result = (|| -> Result<(), String> {
        let root = owned_root(args.get(2).ok_or("Use --run <workspace .tmp directory>")?)?;
        if args.get(1).map(String::as_str) == Some("--handoff") {
            let child = args.iter().any(|arg| arg.starts_with("--cliphost-restart-parent="));
            let outcome = handoff(&root, &args, child);
            if let Err(error) = &outcome {
                write_json(&root.join(if child { "handoff-child-error.json" } else { "handoff-parent-error.json" }),
                    &json!({"ok": false, "pid": std::process::id(), "error": error}))?;
            }
            return outcome;
        }
        if args.get(1).map(String::as_str) == Some("--child") {
            return receiver(&root, args.get(3).ok_or("Child nonce missing")?, &args[4..]);
        }
        if args.get(1).map(String::as_str) != Some("--run") {
            return Err("Unknown probe mode".into());
        }
        #[cfg(windows)]
        let outcome = launch(&root);
        #[cfg(not(windows))]
        let outcome: Result<Value, String> = Err("Windows required".into());
        let report = match outcome {
            Ok(value) => value,
            Err(error) => json!({"ok":false,"error":error}),
        };
        write_json(&root.join("report.json"), &report)?;
        if report["ok"] != true {
            return Err(report.to_string());
        }
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

// The controller explicitly starts the first helper through normal Windows UAC.
// Both processes use the production restart functions, without initializing a
// HostState, scheduled task, clipboard listener, network listener or window.
fn handoff(root: &Path, args: &[String], child: bool) -> Result<(), String> {
    use cliphost_core::process_restart::{launch_new_instance, wait_for_predecessor};
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.canonicalize().map_err(|e| e.to_string())?;
    if !exe.starts_with(root) { return Err("Handoff helper must be copied inside its output directory".into()); }
    let nonce = args.get(3).ok_or("Handoff nonce missing")?;
    let identity = native_identity()?;
    let elevated = cliphost_core::elevation::is_current_process_elevated();
    let atomic = |name: &str, value: &Value| -> Result<(), String> {
        let temporary = root.join(format!("{name}.tmp"));
        write_json(&temporary, value)?;
        fs::rename(temporary, root.join(name)).map_err(|e| e.to_string())
    };
    let await_file = |name: &str| -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !root.join(name).exists() {
            if Instant::now() >= deadline { return Err(format!("Controller did not provide {name}")); }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    };
    if !child {
        if !elevated || identity["integrityRid"].as_u64().unwrap_or(0) < 0x3000 {
            return Err("Handoff requires a genuinely elevated launcher".into());
        }
        for name in ["handoff-parent.json", "handoff-waiting.json", "handoff-ready.json", "allow-parent-exit", "allow-child-exit"] {
            if root.join(name).exists() { return Err("Choose fresh handoff output".into()); }
        }
        std::env::set_var("BETTERCLIP_TAURI_DATA_ROOT", root.join("isolated data"));
        std::env::set_var("INTELLITOOLS_CLIPHOST_TAURI_PORT", "29965");
        std::env::set_var("CLIPHOST_TAURI_RESTART_PROBE_VALUE", nonce);
        std::env::set_var("CLIPHOST_UNRELATED_PROBE_VALUE", nonce);
        atomic("handoff-parent.json", &json!({"pid": std::process::id(), "nonce": nonce,
            "elevated": elevated, "nativeIdentity": identity, "arguments": args}))?;
        launch_new_instance(true)?;
        await_file("allow-parent-exit")?;
        // The replacement must still be blocked on our actual process handle.
        if root.join("handoff-ready.json").exists() { return Err("Replacement skipped live predecessor".into()); }
        atomic("handoff-parent-exiting.json", &json!({"ok": true, "pid": std::process::id(), "nonce": nonce}))?;
        return Ok(());
    }
    atomic("handoff-waiting.json", &json!({"pid": std::process::id(), "nonce": nonce,
        "elevated": elevated, "nativeIdentity": identity}))?;
    let started = Instant::now();
    wait_for_predecessor()?;
    let parent: Value = serde_json::from_slice(&fs::read(root.join("handoff-parent.json")).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let actual_args: Vec<_> = args.iter().skip(1).filter(|arg| !arg.starts_with("--cliphost-restart-parent=")).collect();
    let expected_args = parent["arguments"].as_array().ok_or("Parent arguments missing")?[1..].to_vec();
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?.canonicalize().map_err(|e| e.to_string())?;
    let checks = json!({
        "parentHigh": parent["elevated"] == true && parent["nativeIdentity"]["integrityRid"].as_u64().is_some_and(|rid| rid >= 0x3000),
        "childMedium": !elevated && identity["integrityRid"].as_u64().is_some_and(|rid| (0x2000..0x3000).contains(&rid)),
        "childUsesDesktopParent": identity["desktopPid"].as_u64().is_some_and(|pid| pid != 0) && identity["parentPid"] == identity["desktopPid"],
        "distinctProcesses": parent["pid"].as_u64() != Some(u64::from(std::process::id())),
        "noncePreserved": parent["nonce"] == *nonce && std::env::var("CLIPHOST_TAURI_RESTART_PROBE_VALUE").ok().as_ref() == Some(nonce),
        "argumentsPreserved": serde_json::to_value(actual_args).unwrap() == json!(expected_args),
        "dataRootPreserved": std::env::var_os("BETTERCLIP_TAURI_DATA_ROOT").map(PathBuf::from) == Some(root.join("isolated data")),
        "apiPortPreserved": std::env::var("INTELLITOOLS_CLIPHOST_TAURI_PORT").as_deref() == Ok("29965"),
        "unrelatedEnvironmentNotForwarded": std::env::var_os("CLIPHOST_UNRELATED_PROBE_VALUE").is_none(),
        "workingDirectory": Some(cwd.as_path()) == exe.parent(),
        "parentCompletedLaunch": root.join("handoff-parent-exiting.json").is_file(),
    });
    let ok = checks.as_object().unwrap().values().all(|v| v == true);
    atomic("handoff-ready.json", &json!({"ok": ok, "checks": checks,
        "pid": std::process::id(), "nonce": nonce, "nativeIdentity": identity, "waitMs": started.elapsed().as_millis()}))?;
    await_file("allow-child-exit")?;
    if !ok { return Err("Handoff checks failed".into()); }
    Ok(())
}
