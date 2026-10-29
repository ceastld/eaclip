//! Isolated native Save As navigation acceptance; no clipboard or desktop input.
//! Run from host: cargo run -p cliphost-core --example file_dialog_jump_workflow -- --run
//! A child process owns the real IFileSaveDialog. Its STA timer reads GetFolder /
//! GetFileName, so a successful message send cannot masquerade as navigation.
//! --background-source explicitly skips foreground activation of the source;
//! --run always requires source foreground ownership and never downgrades.
//! --external-dialog <hwnd> <pid> navigates only the explicitly supplied dialog;
//! its actual folder and filename must be inspected separately by the caller.

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    native::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This acceptance workflow requires Windows.");
}

#[cfg(windows)]
mod native {
    use cliphost_core::file_dialog_locate::try_locate_hwnd_guarded;
    use cliphost_core::host::HostState;
    use cliphost_core::model::{ClipKind, ClipRecord};
    use cliphost_core::paths::AppPaths;
    use cliphost_core::store::AppStore;
    use cliphost_core::verb::VerbRunContext;
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use windows::core::{w, Interface, HRESULT, HSTRING};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Ole::IOleWindow;
    use windows::Win32::UI::Shell::{
        FileSaveDialog, IFileSaveDialog, IShellItem, SHCreateItemFromParsingName,
        FOS_DONTADDTORECENT, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, SIGDN_FILESYSPATH,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
        GetWindowThreadProcessId, IsWindow, IsWindowVisible, KillTimer, PeekMessageW, PostMessageW,
        RegisterClassW, SetForegroundWindow, SetTimer, ShowWindow, TranslateMessage, MSG,
        PM_REMOVE, SW_SHOWNORMAL, WM_CLOSE, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
        WS_OVERLAPPEDWINDOW,
    };

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    const FILE_NAME: &str = "cliphost-owned-另存为.txt";
    const CANCELLED: HRESULT = HRESULT(0x800704C7u32 as i32);

    #[derive(Clone, Debug, Deserialize, Serialize)]
    struct DialogState {
        pid: u32,
        hwnd: i64,
        visible: bool,
        folder: String,
        file_name: String,
        elapsed_ms: u128,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct CaptureState {
        source_hwnd: i64,
        foreground_hwnd: i64,
        source_owns_foreground: bool,
        source_visible: bool,
    }

    #[derive(Default, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct CleanupReport {
        window_destroyed: bool,
        cleanup_forced: bool,
        fixture_show_shutdown_issue: bool,
        cancelled: Option<bool>,
    }

    struct Fixture {
        dialog: IFileSaveDialog,
        root: PathBuf,
        started: Instant,
        callback_active: bool,
        closing: bool,
    }

    thread_local! {
        static FIXTURE: RefCell<Option<Fixture>> = const { RefCell::new(None) };
    }

    struct TickGuard;
    impl Drop for TickGuard {
        fn drop(&mut self) {
            FIXTURE.with(|value| {
                if let Some(fixture) = value.borrow_mut().as_mut() {
                    fixture.callback_active = false;
                }
            });
        }
    }

    struct Apartment;
    impl Apartment {
        fn initialize() -> windows::core::Result<Self> {
            unsafe {
                CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
            }
            Ok(Self)
        }
    }
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }

    struct DialogOwner(HWND);
    unsafe extern "system" fn owner_proc(
        hwnd: HWND,
        message: u32,
        wp: WPARAM,
        lp: LPARAM,
    ) -> LRESULT {
        DefWindowProcW(hwnd, message, wp, lp)
    }
    impl DialogOwner {
        fn create() -> Result<Self> {
            Self::create_window(false)
        }

        fn create_source() -> Result<Self> {
            Self::create_window(true)
        }

        fn create_window(source: bool) -> Result<Self> {
            unsafe {
                let instance = GetModuleHandleW(None)?;
                let class = WNDCLASSW {
                    lpfnWndProc: Some(owner_proc),
                    hInstance: instance.into(),
                    lpszClassName: w!("ClipHostOwnedFileDialogFixture"),
                    ..Default::default()
                };
                if RegisterClassW(&class) == 0 {
                    return Err("Could not register owned fixture window.".into());
                }
                // A child uses a hidden timer owner. The parent uses a visible
                // source so menu capture happens while ClipHost owns foreground.
                Ok(Self(CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    class.lpszClassName,
                    w!("ClipHost owned context menu source"),
                    if source {
                        WS_OVERLAPPEDWINDOW
                    } else {
                        WS_OVERLAPPED
                    },
                    if source { 40 } else { 0 },
                    if source { 40 } else { 0 },
                    if source { 320 } else { 0 },
                    if source { 100 } else { 0 },
                    None,
                    None,
                    instance,
                    None,
                )?))
            }
        }
    }
    impl Drop for DialogOwner {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    // Only this process is closed, even after a failed assertion or timeout.
    struct OwnedChild {
        child: Child,
        root: PathBuf,
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = std::fs::write(self.root.join("stop"), b"close owned fixture");
            for _ in 0..20 {
                if self.child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    pub fn main() -> Result<()> {
        let args: Vec<String> = std::env::args().collect();
        match args.get(1).map(String::as_str) {
            Some("--run") if args.len() == 2 => run(false),
            Some("--background-source") if args.len() == 2 => run(true),
            Some("--run") if args.len() == 3 && args[2] == "--background-source" => run(true),
            Some("--dialog-fixture") if args.len() == 3 => {
                let root = checked_fixture_root(Path::new(&args[2]))?;
                show_fixture(root)
            }
            Some("--external-dialog") if args.len() == 4 => {
                run_external_dialog(args[2].parse()?, args[3].parse()?)
            }
            _ => Err("Use --run, --background-source or --external-dialog <hwnd> <pid> for explicit native Save As acceptance.".into()),
        }
    }

    fn temporary_base() -> Result<PathBuf> {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&base)?;
        // Exercise normal shell paths, rather than Windows' verbatim canonical prefix.
        let canonical = base.canonicalize()?;
        Ok(PathBuf::from(
            canonical.to_string_lossy().trim_start_matches(r"\\?\"),
        ))
    }

    fn checked_fixture_root(path: &Path) -> Result<PathBuf> {
        let root = path.canonicalize()?;
        let base = temporary_base()?.canonicalize()?;
        if root == base || !root.starts_with(base) {
            return Err("Fixture data must be a child directory of clip/.tmp.".into());
        }
        Ok(PathBuf::from(
            root.to_string_lossy().trim_start_matches(r"\\?\"),
        ))
    }

    fn run(background_source: bool) -> Result<()> {
        // Native navigation is explicit. HostState below is opened headless
        // against only this workflow's data, without hooks or monitoring.
        cliphost_core::clipboard::force_headless(false);
        if cliphost_core::clipboard::is_headless() {
            return Err("Set CLIPHOST_TAURI_HEADLESS=0 for this explicit native workflow.".into());
        }
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let root =
            temporary_base()?.join(format!("file-dialog-jump-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        let initial = root.join("initial");
        let first = root.join("first folder");
        let second = root.join("第二个目录");
        for path in [&initial, &first, &second] {
            std::fs::create_dir(path)?;
        }
        let mut owned = start_fixture(&root)?;
        let decoy_root = root.join("decoy");
        std::fs::create_dir_all(decoy_root.join("initial"))?;
        let mut decoy = start_fixture(&decoy_root)?;
        let mut states = Vec::new();
        let mut decoy_states = Vec::new();
        let mut capture_states = Vec::new();
        let navigation = exercise(
            &mut owned,
            &mut decoy,
            [&initial, &first, &second],
            &mut states,
            &mut decoy_states,
            background_source,
            &mut capture_states,
        );
        let cleanup = states
            .first()
            .map(|state| state.hwnd)
            .ok_or_else(|| "No accepted fixture HWND to close.".into())
            .and_then(|hwnd| close_fixture(&mut owned, hwnd));
        let decoy_cleanup = decoy_states
            .first()
            .map(|state| state.hwnd)
            .ok_or_else(|| "No accepted decoy HWND to close.".into())
            .and_then(|hwnd| close_fixture(&mut decoy, hwnd));
        let mut receipt = json!({
            "navigationPassed": navigation.is_ok(), "fixture_pid": owned.child.id(), "states": states,
            "decoy_pid": decoy.child.id(), "decoyStates": decoy_states,
            "backgroundSource": background_source,
            "sourceOwnsForegroundAtCapture": (!capture_states.is_empty())
                .then(|| capture_states.iter().all(|state| state.source_owns_foreground)),
            "captureStates": capture_states,
            "navigationError": navigation.as_ref().err().map(|error| error.to_string()),
            "cleanupError": cleanup.as_ref().err().map(|error| error.to_string()),
            "decoyCleanup": decoy_cleanup.as_ref().ok(),
            "decoyCleanupError": decoy_cleanup.as_ref().err().map(|error| error.to_string()),
            "assertions": ["external process", "exact owned HWND", "two actual folders",
                "HostState production reveal_item_native entry",
                if background_source { "explicit background source capture" } else { "ClipHost source owns foreground at capture" },
                "second visible Save As dialog remains unchanged", "original filename preserved",
                "dialog remains open", "retired native menu request rejects navigation",
                "retired guard rejects navigation"],
            "clipboard_access": false, "desktop_input": false, "product_data_access": false,
            "isolatedHostDataRoot": root.join("data"),
        });
        if let Ok(cleanup) = &cleanup {
            receipt
                .as_object_mut()
                .unwrap()
                .extend(serde_json::to_value(cleanup)?.as_object().unwrap().clone());
        }
        std::fs::write(
            root.join("verification.json"),
            serde_json::to_vec_pretty(&receipt)?,
        )?;
        println!("{}", root.join("verification.json").display());
        navigation?;
        cleanup?;
        decoy_cleanup.map(|_| ())
    }

    fn start_fixture(root: &Path) -> Result<OwnedChild> {
        let stdout = std::fs::File::create(root.join("fixture.stdout.log"))?;
        let stderr = std::fs::File::create(root.join("fixture.stderr.log"))?;
        let child = Command::new(std::env::current_exe()?)
            .arg("--dialog-fixture")
            .arg(root)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()?;
        Ok(OwnedChild {
            child,
            root: root.to_path_buf(),
        })
    }

    fn prepare_reveal_host(root: &Path, folders: &[&Path]) -> Result<(HostState, Vec<String>)> {
        let app_paths = AppPaths::from_data_root(root.join("data"));
        app_paths.ensure()?;
        let mut ids = Vec::new();
        {
            let store = AppStore::open(&app_paths.db)?;
            for folder in folders {
                let file = folder.join("context-menu-fixture.txt");
                std::fs::write(&file, b"owned file for folder reveal")?;
                let mut item = ClipRecord::new_text("", "owned-file-dialog-fixture");
                item.kind = ClipKind::FileClipItem;
                item.file_paths = vec![file.to_string_lossy().into_owned()];
                ids.push(item.id.clone());
                store.insert_clip(&item, true)?;
            }
        }
        cliphost_core::clipboard::force_headless(true);
        let opened = HostState::open(app_paths, 29065);
        cliphost_core::clipboard::force_headless(false);
        Ok((opened?, ids))
    }

    fn run_external_dialog(hwnd: i64, pid: u32) -> Result<()> {
        cliphost_core::clipboard::force_headless(false);
        if cliphost_core::clipboard::is_headless() {
            return Err("Set CLIPHOST_TAURI_HEADLESS=0 for explicit external navigation.".into());
        }
        if hwnd == 0 || pid == 0 || pid == std::process::id() {
            return Err("Supply the exact HWND and PID of an external file dialog.".into());
        }
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let root = temporary_base()?.join(format!(
            "file-dialog-external-{}-{nonce}",
            std::process::id(),
        ));
        std::fs::create_dir_all(&root)?;
        let folders = [root.join("Chrome 首次目录"), root.join("Chrome 第二次目录")];
        let initial_foreground = unsafe { GetForegroundWindow().0 as i64 };
        let mut responses = Vec::new();
        let navigation = (|| -> Result<()> {
            verify_owned_window(hwnd, pid)?;
            if !cliphost_core::file_dialog_locate::is_file_dialog_hwnd(hwnd) {
                return Err("The supplied external HWND is not a recognized file dialog.".into());
            }
            for folder in &folders {
                std::fs::create_dir(folder)?;
            }
            let folder_refs: Vec<&Path> = folders.iter().map(PathBuf::as_path).collect();
            let (host, ids) = prepare_reveal_host(&root, &folder_refs)?;
            // This HWND supplies only the production page identity. Keep it
            // hidden and never activate it while the caller inspects Chrome.
            let source = DialogOwner::create_source()?;
            let source_hwnd = source.0;
            host.set_host_hwnd(source_hwnd.0 as isize);
            host.set_window_runtime_ready(true);
            host.set_clipboard_web_view_ready(true);
            host.set_shell_open_runner(|_| {
                Err("External-dialog acceptance must not open Explorer.".into())
            });
            let context = VerbRunContext {
                target_window_handle: hwnd,
                ..Default::default()
            };
            host.execute_verb_with_context("clip show", Some(&context))?;
            for (folder, id) in folders.iter().zip(&ids) {
                verify_owned_window(hwnd, pid)?;
                if !cliphost_core::file_dialog_locate::is_file_dialog_hwnd(hwnd) {
                    return Err("The supplied external dialog is no longer navigable.".into());
                }
                cliphost_core::active_window::record_resolved_root_hwnd(hwnd as isize);
                let request = host.capture_native_reveal()?;
                println!("Target folder: {}", folder.display());
                let response =
                    host.reveal_item_native(&json!({ "id": id, "target": "folder" }), &request);
                let recorded = match &response {
                    Ok(value) => json!({"folder": folder, "response": value}),
                    Err(error) => json!({"folder": folder, "error": error}),
                };
                println!("{}", serde_json::to_string(&recorded)?);
                responses.push(recorded);
                response?;
                verify_owned_window(hwnd, pid)?;
            }
            Ok(())
        })();
        let receipt = json!({
            "mode": "external-dialog", "targetHwnd": hwnd, "targetPid": pid,
            "navigationAccepted": navigation.is_ok(), "responses": responses,
            "navigationError": navigation.as_ref().err().map(|error| error.to_string()),
            "targetFolders": folders, "isolatedHostDataRoot": root.join("data"),
            "initialForegroundHwnd": initial_foreground,
            "finalForegroundHwnd": unsafe { GetForegroundWindow().0 as i64 },
            "targetStillVisible": verify_owned_window(hwnd, pid).is_ok(),
            "sourceActivated": false, "externalDialogClosed": false,
            "externalProcessTerminated": false, "guiVerificationRequired": true,
            "clipboard_access": false, "desktop_input": false, "product_data_access": false,
        });
        let receipt_path = root.join("verification.json");
        std::fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt)?)?;
        println!("Receipt: {}", receipt_path.display());
        navigation
    }

    fn exercise(
        owned: &mut OwnedChild,
        decoy: &mut OwnedChild,
        paths: [&Path; 3],
        states: &mut Vec<DialogState>,
        decoy_states: &mut Vec<DialogState>,
        background_source: bool,
        capture_states: &mut Vec<CaptureState>,
    ) -> Result<()> {
        let initial = wait_for_state(owned, paths[0], None, None)?;
        let hwnd = initial.hwnd;
        let pid = owned.child.id();
        states.push(initial);
        let decoy_initial = decoy.root.join("initial");
        decoy_states.push(wait_for_state(decoy, &decoy_initial, None, None)?);

        let (host, ids) = prepare_reveal_host(&owned.root, &paths[1..])?;
        let source = DialogOwner::create_source()?;
        let source_hwnd = source.0;
        host.set_host_hwnd(source_hwnd.0 as isize);
        host.set_window_runtime_ready(true);
        host.set_clipboard_web_view_ready(true);
        let context = VerbRunContext {
            target_window_handle: hwnd,
            ..Default::default()
        };
        host.execute_verb_with_context("clip show", Some(&context))?;

        for (path, id) in paths[1..].iter().zip(&ids) {
            verify_owned_window(hwnd, pid)?;
            prepare_source_capture(source.0, background_source, capture_states)?;
            let request = host.capture_native_reveal()?;
            let response =
                host.reveal_item_native(&json!({ "id": id, "target": "folder" }), &request)?;
            if response["ok"] != true {
                return Err(format!("Reveal failed: {response}").into());
            }
            states.push(wait_for_state(owned, path, Some(hwnd), None)?);
            let previous = decoy_states.last().unwrap();
            decoy_states.push(wait_for_state(
                decoy,
                &decoy_initial,
                Some(previous.hwnd),
                Some(previous.elapsed_ms),
            )?);
        }

        prepare_source_capture(source.0, background_source, capture_states)?;
        let retired_request = host.capture_native_reveal()?;
        host.window_hide();
        host.execute_verb_with_context("clip show", Some(&context))?;
        if host
            .reveal_item_native(
                &json!({ "id": ids[0], "target": "folder" }),
                &retired_request,
            )
            .is_ok()
        {
            return Err("A retired native context menu request navigated.".into());
        }
        let rejected = try_locate_hwnd_guarded(hwnd, &paths[0].to_string_lossy(), || {
            Err("owned fixture menu retired".to_string())
        });
        if rejected.as_ref().err().map(String::as_str) != Some("owned fixture menu retired") {
            return Err(format!("Retired guard was not rejected: {rejected:?}").into());
        }
        // Require a fresh STA read after rejection, rather than reusing the last snapshot.
        let after_rejection: DialogState =
            serde_json::from_slice(&std::fs::read(owned.root.join("state.json"))?)?;
        let final_state = wait_for_state(
            owned,
            paths[2],
            Some(hwnd),
            Some(after_rejection.elapsed_ms),
        )?;
        states.push(final_state);
        let previous = decoy_states.last().unwrap();
        decoy_states.push(wait_for_state(
            decoy,
            &decoy_initial,
            Some(previous.hwnd),
            Some(previous.elapsed_ms),
        )?);
        Ok(())
    }

    fn prepare_source_capture(
        source: HWND,
        background_source: bool,
        states: &mut Vec<CaptureState>,
    ) -> Result<()> {
        if !background_source {
            activate_source(source)?;
        }
        let foreground = unsafe { GetForegroundWindow() };
        states.push(CaptureState {
            source_hwnd: source.0 as i64,
            foreground_hwnd: foreground.0 as i64,
            source_owns_foreground: source == foreground,
            source_visible: unsafe { IsWindowVisible(source).as_bool() },
        });
        Ok(())
    }

    fn activate_source(source: HWND) -> Result<()> {
        unsafe {
            let _ = ShowWindow(source, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(source);
        }
        let deadline = Instant::now() + Duration::from_millis(900);
        loop {
            unsafe {
                let mut message = MSG::default();
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                if GetForegroundWindow() == source {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err("Owned ClipHost menu source did not acquire foreground.".into());
            }
            std::thread::sleep(Duration::from_millis(12));
        }
    }

    fn close_fixture(owned: &mut OwnedChild, hwnd: i64) -> Result<CleanupReport> {
        verify_owned_window(hwnd, owned.child.id())?;
        // Close only the exact fixture window; no desktop input is generated.
        unsafe {
            PostMessageW(HWND(hwnd as *mut _), WM_CLOSE, WPARAM(0), LPARAM(0))?;
        }
        std::fs::write(
            owned.root.join("stop"),
            b"close owned fixture after navigation",
        )?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut report = CleanupReport::default();
        loop {
            let handle = HWND(hwnd as *mut _);
            let mut window_pid = 0;
            unsafe {
                GetWindowThreadProcessId(handle, Some(&mut window_pid));
            }
            report.window_destroyed =
                unsafe { !IsWindow(handle).as_bool() } || window_pid != owned.child.id();
            if let Some(status) = owned.child.try_wait()? {
                if !status.success() {
                    return Err(format!("Owned fixture exited with {status}").into());
                }
                let closed: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(owned.root.join("closed.json"))?)?;
                report.cancelled = closed["cancelled"].as_bool();
                if report.cancelled != Some(true) || !report.window_destroyed {
                    return Err(format!(
                        "Fixture did not return cancellation after window destruction: {closed}"
                    )
                    .into());
                }
                break;
            }
            if Instant::now() >= deadline {
                if !report.window_destroyed {
                    return Err("Owned fixture window did not close after acceptance.".into());
                }
                // On this shell, even the no-navigation fixture can destroy
                // its window without unwinding Show. Preserve that limitation
                // explicitly and terminate only the child created above.
                report.cleanup_forced = true;
                report.fixture_show_shutdown_issue = true;
                owned.child.kill()?;
                let _ = owned.child.wait()?;
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        Ok(report)
    }

    fn verify_owned_window(hwnd: i64, pid: u32) -> std::result::Result<(), String> {
        let handle = HWND(hwnd as *mut _);
        let mut actual_pid = 0;
        unsafe {
            GetWindowThreadProcessId(handle, Some(&mut actual_pid));
            if !IsWindow(handle).as_bool()
                || !IsWindowVisible(handle).as_bool()
                || actual_pid != pid
            {
                return Err("The exact owned fixture window is no longer current.".into());
            }
        }
        Ok(())
    }

    fn same_path(actual: &str, expected: &Path) -> bool {
        let normalize = |value: &str| {
            value
                .trim_start_matches(r"\\?\")
                .trim_end_matches(['\\', '/'])
                .replace('/', "\\")
                .to_lowercase()
        };
        normalize(actual) == normalize(&expected.to_string_lossy())
    }

    fn wait_for_state(
        owned: &mut OwnedChild,
        expected: &Path,
        hwnd: Option<i64>,
        after_elapsed_ms: Option<u128>,
    ) -> Result<DialogState> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut last = String::new();
        loop {
            if let Some(status) = owned.child.try_wait()? {
                return Err(format!(
                    "Owned Save As fixture exited early: {status}; last state: {last}"
                )
                .into());
            }
            if let Ok(bytes) = std::fs::read(owned.root.join("state.json")) {
                if let Ok(state) = serde_json::from_slice::<DialogState>(&bytes) {
                    last = format!("{state:?}");
                    if state.pid == owned.child.id()
                        && state.hwnd != 0
                        && state.visible
                        && hwnd.is_none_or(|value| state.hwnd == value)
                        && after_elapsed_ms.is_none_or(|value| state.elapsed_ms > value)
                        && same_path(&state.folder, expected)
                        && state.file_name == FILE_NAME
                    {
                        verify_owned_window(state.hwnd, state.pid)?;
                        return Ok(state);
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "Actual folder/filename did not match {} / {FILE_NAME}; last state: {last}",
                    expected.display()
                )
                .into());
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    fn show_fixture(root: PathBuf) -> Result<()> {
        let _apartment = Apartment::initialize()?;
        let owner = DialogOwner::create()?;
        let dialog: IFileSaveDialog =
            unsafe { CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)? };
        unsafe {
            dialog.SetOptions(
                dialog.GetOptions()? | FOS_FORCEFILESYSTEM | FOS_DONTADDTORECENT | FOS_NOCHANGEDIR,
            )?;
            dialog.SetTitle(&HSTRING::from("ClipHost owned Save As navigation fixture"))?;
            dialog.SetFileName(&HSTRING::from(FILE_NAME))?;
            let initial = HSTRING::from(root.join("initial").to_string_lossy().as_ref());
            let folder: IShellItem = SHCreateItemFromParsingName(&initial, None)?;
            dialog.SetFolder(&folder)?;
        }
        FIXTURE.with(|value| {
            *value.borrow_mut() = Some(Fixture {
                dialog: dialog.clone(),
                root: root.clone(),
                started: Instant::now(),
                callback_active: false,
                closing: false,
            })
        });
        let timer = unsafe { SetTimer(owner.0, 1, 80, Some(tick)) };
        if timer == 0 {
            FIXTURE.with(|value| value.borrow_mut().take());
            return Err("Could not install owned Save As fixture timer.".into());
        }
        let shown = unsafe { dialog.Show(owner.0) };
        let cancelled = shown
            .as_ref()
            .err()
            .is_some_and(|error| error.code() == CANCELLED);
        // Record Show's return before releasing any COM references, so teardown
        // failures cannot be confused with a dialog that is still modal.
        std::fs::write(
            root.join("closed.json"),
            serde_json::to_vec_pretty(&json!({
                "cancelled": cancelled, "show_result": format!("{shown:?}"),
            }))?,
        )?;
        unsafe {
            // The stop callback kills this timer before posting cancellation.
            let _ = KillTimer(owner.0, timer);
        }
        FIXTURE.with(|value| value.borrow_mut().take());
        if !cancelled {
            return Err(format!("Owned dialog closed unexpectedly: {shown:?}").into());
        }
        Ok(())
    }

    unsafe extern "system" fn tick(owner: HWND, _message: u32, timer: usize, _time: u32) {
        // Sampling can pump COM messages. Never enter a second
        // timer callback while the first callback is still inside the dialog.
        let fixture = FIXTURE.with(|value| {
            let mut value = value.borrow_mut();
            let fixture = value.as_mut()?;
            if fixture.callback_active || fixture.closing {
                return None;
            }
            fixture.callback_active = true;
            Some((
                fixture.dialog.clone(),
                fixture.root.clone(),
                fixture.started,
            ))
        });
        let Some((dialog, root, started)) = fixture else {
            return;
        };
        let _callback = TickGuard;
        if root.join("stop").exists() || started.elapsed() > Duration::from_secs(90) {
            FIXTURE.with(|value| {
                if let Some(fixture) = value.borrow_mut().as_mut() {
                    fixture.closing = true;
                }
            });
            let killed = KillTimer(owner, timer);
            let close_log = root.join("close-request.json");
            // IFileDialog.Close returned S_OK inside this timer but did not
            // unwind Show on the tested shell. Queue normal cancellation to
            // this process's exact dialog, after the timer callback returns.
            let target = dialog.cast::<IOleWindow>().and_then(|ole| ole.GetWindow());
            let posted = target.map(|hwnd| {
                if verify_owned_window(hwnd.0 as i64, std::process::id()).is_ok() {
                    // This message is never sent to user windows.
                    format!("{:?}", PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0)))
                } else {
                    "owned dialog is no longer current".to_string()
                }
            });
            let _ = std::fs::write(
                close_log,
                json!({
                    "phase": "cancellation_posted", "timer_killed": killed.is_ok(),
                    "result": format!("{posted:?}"), "elapsed_ms": started.elapsed().as_millis(),
                })
                .to_string(),
            );
            return;
        }
        let Ok(ole) = dialog.cast::<IOleWindow>() else {
            return;
        };
        let Ok(hwnd) = ole.GetWindow() else {
            return;
        };
        let Ok(folder) = dialog.GetFolder() else {
            return;
        };
        let Ok(folder_name) = folder.GetDisplayName(SIGDN_FILESYSPATH) else {
            return;
        };
        let folder = folder_name.to_string().unwrap_or_default();
        CoTaskMemFree(Some(folder_name.0 as *const _));
        let Ok(file_name) = dialog.GetFileName() else {
            return;
        };
        let name = file_name.to_string().unwrap_or_default();
        CoTaskMemFree(Some(file_name.0 as *const _));
        let state = DialogState {
            pid: std::process::id(),
            hwnd: hwnd.0 as i64,
            visible: IsWindowVisible(hwnd).as_bool(),
            folder,
            file_name: name,
            elapsed_ms: started.elapsed().as_millis(),
        };
        if let Ok(bytes) = serde_json::to_vec_pretty(&state) {
            // Readers see a complete old or new snapshot; transient replacement
            // failures keep the previous state and are retried by the next tick.
            let pending = root.join("state.next.json");
            if std::fs::write(&pending, bytes).is_ok() {
                let _ = std::fs::rename(pending, root.join("state.json"));
            }
        }
    }
}
