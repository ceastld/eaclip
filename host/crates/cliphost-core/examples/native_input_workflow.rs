//! Explicit native-input acceptance test, separate from headless cargo tests.
//! cargo run -p cliphost-core --example native_input_workflow -- --run <temporary-root>
//! Creates two dedicated receiver windows; never finds or types into user windows.
//! --hook-input <temporary-root> verifies generated input through real hooks without clipboard writes.
//! --hook-click <temporary-root> verifies pinned no-activate clicks through real mouse/keyboard hooks.

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    native::main()
}
#[cfg(not(windows))]
fn main() {
    eprintln!("This acceptance test requires Windows.");
}

#[cfg(windows)]
mod native {
    use cliphost_core::{
        clipboard, hooks::InputHooks, input_target::InputTarget, paste, AppPaths,
        ChromeCommand, ClipRecord, HostState,
    };
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Write};
    use std::path::PathBuf;
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::{
        atomic::{AtomicIsize, AtomicU32, AtomicUsize, Ordering},
        mpsc, Mutex, OnceLock,
    };
    use std::time::{Duration, Instant};
    use windows::{
        core::w,
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
            Graphics::Gdi::HBRUSH,
            System::DataExchange::GetClipboardSequenceNumber,
            System::LibraryLoader::GetModuleHandleW,
            UI::{
                HiDpi::{
                    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                },
                Input::KeyboardAndMouse::{
                    GetAsyncKeyState, GetFocus, GetKeyState, SendInput, SetFocus, INPUT, INPUT_0, INPUT_MOUSE,
                    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT, VK_CONTROL, VK_LBUTTON,
                    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
                },
                WindowsAndMessaging::*,
            },
        },
    };

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    static FRAMES: [AtomicIsize; 2] = [AtomicIsize::new(0), AtomicIsize::new(0)];
    static EDITS: [AtomicIsize; 2] = [AtomicIsize::new(0), AtomicIsize::new(0)];
    static SWITCH_LENGTH: AtomicUsize = AtomicUsize::new(usize::MAX);
    static DRIVER_PID: AtomicU32 = AtomicU32::new(0);
    static COMMANDS: OnceLock<Mutex<mpsc::Receiver<Value>>> = OnceLock::new();
    static STARTED: OnceLock<Instant> = OnceLock::new();
    static KEY_EVENTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
    static FOREGROUND_GRANT: Mutex<String> = Mutex::new(String::new());
    static HOST_MOUSE_DOWN: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
    static HOST_MOUSE_UP: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

    fn hwnd(value: isize) -> HWND {
        HWND(value as *mut _)
    }
    fn frame(index: usize) -> HWND {
        hwnd(FRAMES[index].load(Ordering::Relaxed))
    }
    fn edit(index: usize) -> HWND {
        hwnd(EDITS[index].load(Ordering::Relaxed))
    }
    fn reply(value: Value) {
        println!("{value}");
        let _ = std::io::stdout().flush();
    }

    unsafe fn activate(index: usize) -> bool {
        let result = SetForegroundWindow(frame(index)).as_bool();
        let _ = SetFocus(edit(index));
        let active = result && GetForegroundWindow() == frame(index);
        if active {
            *FOREGROUND_GRANT.lock().unwrap() = format!("{:?}", AllowSetForegroundWindow(DRIVER_PID.load(Ordering::Relaxed)));
        }
        active
    }

    unsafe fn read_text(index: usize) -> String {
        let mut text = vec![0u16; GetWindowTextLengthW(edit(index)).max(0) as usize + 1];
        let length = GetWindowTextW(edit(index), &mut text).max(0) as usize;
        String::from_utf16_lossy(&text[..length])
    }

    unsafe fn execute(command: Value) -> Result<()> {
        let index = command["target"].as_u64().unwrap_or(0) as usize;
        if index >= 2 {
            return Err("invalid receiver index".into());
        }
        let value = match command["command"].as_str().unwrap_or("") {
            "activate" => json!({"activated": activate(index)}),
            "clear" => {
                SetWindowTextW(edit(index), w!(""))?;
                KEY_EVENTS.lock().unwrap().clear();
                json!({})
            }
            "read" => {
                json!({"text": read_text(index), "maximized": IsZoomed(frame(index)).as_bool(), "minimized": IsIconic(frame(index)).as_bool(), "keyboard": *KEY_EVENTS.lock().unwrap(),
                    "focus": GetFocus().0 as isize, "edits": [edit(0).0 as isize, edit(1).0 as isize],
                    "foreground": GetForegroundWindow().0 as isize, "windows": [frame(0).0 as isize, frame(1).0 as isize], "foregroundGrant": *FOREGROUND_GRANT.lock().unwrap()})
            }
            "maximize" => {
                let _ = ShowWindow(frame(index), SW_MAXIMIZE);
                json!({})
            }
            "minimize" => {
                let _ = ShowWindow(frame(index), SW_MINIMIZE);
                json!({})
            }
            "normal" => {
                let _ = ShowWindow(frame(index), SW_RESTORE);
                json!({})
            }
            "switchAfterChars" => {
                SWITCH_LENGTH.store(
                    command["count"].as_u64().unwrap_or(3) as usize,
                    Ordering::Relaxed,
                );
                json!({})
            }
            "switchAfterDelay" => {
                SetTimer(
                    frame(0),
                    2,
                    command["milliseconds"].as_u64().unwrap_or(80) as u32,
                    None,
                );
                json!({})
            }
            "quit" => {
                PostQuitMessage(0);
                json!({})
            }
            _ => return Err("unknown receiver command".into()),
        };
        reply(json!({"ok": true, "result": value}));
        Ok(())
    }

    unsafe extern "system" fn window_proc(
        window: HWND,
        msg: u32,
        wp: WPARAM,
        lp: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_ACTIVATE if wp.0 & 0xffff != 0 => {
                // A real ClipHost click gives the sender foreground permission.
                // This receiver grants that permission only to its test driver.
                let _ = AllowSetForegroundWindow(DRIVER_PID.load(Ordering::Relaxed));
            }
            WM_SETFOCUS => {
                let child = hwnd(GetWindowLongPtrW(window, GWLP_USERDATA));
                if !child.0.is_null() {
                    let _ = SetFocus(child);
                }
                return LRESULT(0);
            }
            WM_COMMAND if (wp.0 >> 16) & 0xffff == 0x0300 && hwnd(lp.0) == edit(0) => {
                let limit = SWITCH_LENGTH.load(Ordering::Relaxed);
                if limit != usize::MAX && GetWindowTextLengthW(edit(0)) as usize >= limit {
                    SWITCH_LENGTH.store(usize::MAX, Ordering::Relaxed);
                    activate(1);
                }
            }
            WM_TIMER if wp.0 == 2 => {
                let _ = KillTimer(window, 2);
                activate(1);
            }
            WM_TIMER => {
                if STARTED
                    .get()
                    .is_some_and(|t| t.elapsed() > Duration::from_secs(90))
                {
                    PostQuitMessage(1);
                }
                loop {
                    let command = COMMANDS.get().unwrap().lock().unwrap().try_recv();
                    match command {
                        Ok(command) => {
                            if let Err(error) = execute(command) {
                                reply(json!({"ok": false, "error": error.to_string()}));
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            _ => {}
        }
        DefWindowProcW(window, msg, wp, lp)
    }

    fn receiver(driver_pid: u32) -> Result<()> {
        DRIVER_PID.store(driver_pid, Ordering::Relaxed);
        let (send, receive) = mpsc::channel();
        COMMANDS
            .set(Mutex::new(receive))
            .map_err(|_| "receiver already initialized")?;
        STARTED.set(Instant::now()).unwrap();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else {
                    break;
                };
                let Ok(value) = serde_json::from_str(&line) else {
                    break;
                };
                if send.send(value).is_err() {
                    return;
                }
            }
            let _ = send.send(json!({"command": "quit"}));
        });
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                lpszClassName: w!("ClipHostNativeInputReceiver"),
                hbrBackground: HBRUSH(6usize as *mut _),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err("RegisterClassW failed".into());
            }
            for index in 0..2 {
                let title = if index == 0 {
                    w!("Rust paste test — A")
                } else {
                    w!("Rust paste test — B")
                };
                let window = CreateWindowExW(
                    WS_EX_TOPMOST,
                    class.lpszClassName,
                    title,
                    WS_OVERLAPPEDWINDOW,
                    100 + index as i32 * 360,
                    100,
                    350,
                    180,
                    None,
                    None,
                    instance,
                    None,
                )?;
                FRAMES[index].store(window.0 as isize, Ordering::Relaxed);
                let child = CreateWindowExW(
                    WS_EX_CLIENTEDGE,
                    w!("EDIT"),
                    w!(""),
                    WS_CHILD
                        | WS_VISIBLE
                        | WS_TABSTOP
                        | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32),
                    8,
                    8,
                    310,
                    115,
                    window,
                    None,
                    instance,
                    None,
                )?;
                EDITS[index].store(child.0 as isize, Ordering::Relaxed);
                SetWindowLongPtrW(window, GWLP_USERDATA, child.0 as isize);
                let _ = ShowWindow(
                    window,
                    if index == 0 {
                        SW_SHOWNORMAL
                    } else {
                        SW_SHOWNOACTIVATE
                    },
                );
            }
            SetTimer(frame(0), 1, 10, None);
            reply(
                json!({"ready": true, "pid": std::process::id(), "windows": [frame(0).0 as isize, frame(1).0 as isize]}),
            );
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                if matches!(msg.message, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP | WM_CHAR) {
                    let mut events = KEY_EVENTS.lock().unwrap();
                    if events.len() >= 48 { events.remove(0); }
                    events.push(json!({"message": msg.message, "key": msg.wParam.0, "flags": (msg.lParam.0 >> 16) & 0xffff,
                        "extraInfo": GetMessageExtraInfo().0,
                        "shift": GetKeyState(VK_SHIFT.0 as i32) < 0, "ctrl": GetKeyState(VK_CONTROL.0 as i32) < 0,
                        "target": usize::from(msg.hwnd == edit(1))}));
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = DestroyWindow(frame(1));
            let _ = DestroyWindow(frame(0));
        }
        Ok(())
    }

    struct ReceiverProcess {
        child: Child,
        input: ChildStdin,
        output: mpsc::Receiver<Value>,
        windows: [isize; 2],
        previous_foreground: isize,
    }

    impl ReceiverProcess {
        fn start() -> Result<Self> {
            let previous_foreground = unsafe { GetForegroundWindow().0 as isize };
            let mut child = Command::new(std::env::current_exe()?)
                .arg("--receiver")
                .arg(std::process::id().to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?;
            let input = child.stdin.take().unwrap();
            let output = child.stdout.take().unwrap();
            let (send, receive) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(output)
                    .lines()
                    .map_while(std::result::Result::ok)
                {
                    if let Ok(value) = serde_json::from_str(&line) {
                        if send.send(value).is_err() {
                            break;
                        }
                    }
                }
            });
            let mut fixture = Self {
                child,
                input,
                output: receive,
                windows: [0; 2],
                previous_foreground,
            };
            let ready = fixture.output.recv_timeout(Duration::from_secs(30))?;
            assert_eq!(ready["pid"].as_u64(), Some(fixture.child.id() as u64));
            for index in 0..2 {
                fixture.windows[index] = ready["windows"][index]
                    .as_i64()
                    .ok_or("missing receiver window")?
                    as isize;
                let mut pid = 0;
                unsafe {
                    GetWindowThreadProcessId(hwnd(fixture.windows[index]), Some(&mut pid));
                }
                assert_eq!(pid, fixture.child.id(), "never target a foreign window");
            }
            Ok(fixture)
        }

        fn command(&mut self, value: Value) -> Result<Value> {
            writeln!(self.input, "{value}")?;
            self.input.flush()?;
            let response = self.output.recv_timeout(Duration::from_secs(5))?;
            if response["ok"] != true {
                return Err(response.to_string().into());
            }
            Ok(response["result"].clone())
        }

        fn activate(&mut self, index: usize) -> Result<InputTarget> {
            // Test setup models the user selecting one of our receiver windows.
            // The currently foreground receiver activates its sibling and grants
            // the driver permission; product restoration is tested separately.
            let result = self.command(json!({"command": "activate", "target": index}))?;
            if result["activated"] != true {
                return Err(format!("receiver could not activate its own window {index}: {result}").into());
            }
            let target = InputTarget::capture(self.windows[index])?;
            target.verify()?;
            Ok(target)
        }

        fn restore_target(&mut self, target: InputTarget, state: &str) -> Result<()> {
            if let Err(error) = target.activate() {
                let immediate = self.command(json!({"command": "read"}))?;
                std::thread::sleep(Duration::from_millis(250));
                let delayed = self.command(json!({"command": "read"}))?;
                return Err(format!("restore {state} receiver: {error}; immediate={immediate}; delayed={delayed}").into());
            }
            Ok(())
        }

        fn initial_focus(&mut self) -> Result<()> {
            // The ready line precedes the receiver's first message-loop turn.
            self.command(json!({"command": "read"}))?;
            if self.activate(0).is_ok() {
                return Ok(());
            }
            // Background launch has no foreground permission. Model the user's
            // initial click, but only if the hit-tested point is our own window.
            // Never click an occluding application or relax production guards.
            unsafe {
                if GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 {
                    return Err("mouse button is held".into());
                }
                let window = hwnd(self.windows[0]);
                let mut point = POINT { x: 30, y: 30 };
                if !windows::Win32::Graphics::Gdi::ClientToScreen(window, &mut point).as_bool() {
                    return Err("cannot locate receiver client".into());
                }
                let deadline = Instant::now() + Duration::from_millis(500);
                while GetAncestor(WindowFromPoint(point), GA_ROOT) != window
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                if GetAncestor(WindowFromPoint(point), GA_ROOT) != window {
                    let mut rect = RECT::default();
                    GetWindowRect(window, &mut rect)?;
                    let hit = WindowFromPoint(point);
                    let root = GetAncestor(hit, GA_ROOT);
                    let mut hit_pid = 0;
                    GetWindowThreadProcessId(hit, Some(&mut hit_pid));
                    let mut class = [0u16; 256];
                    let class_len = GetClassNameW(hit, &mut class).max(0) as usize;
                    eprintln!(
                        "Focus diagnostic: {}",
                        json!({
                            "receiver": window.0 as isize,
                            "receiverPid": self.child.id(),
                            "visible": IsWindowVisible(window).as_bool(),
                            "rect": [rect.left, rect.top, rect.right, rect.bottom],
                            "point": [point.x, point.y],
                            "hit": hit.0 as isize,
                            "hitRoot": root.0 as isize,
                            "hitPid": hit_pid,
                            "hitClass": String::from_utf16_lossy(&class[..class_len]),
                            "foreground": GetForegroundWindow().0 as isize,
                        })
                    );
                    return Err(
                        "receiver is occluded; refusing to click another application".into(),
                    );
                }
                let mut previous = POINT::default();
                GetCursorPos(&mut previous)?;
                let clicked = (|| -> Result<()> {
                    SetCursorPos(point.x, point.y)?;
                    if GetAncestor(WindowFromPoint(point), GA_ROOT) != window {
                        return Err("receiver became occluded".into());
                    }
                    let input = |flags| INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                dwFlags: flags,
                                ..Default::default()
                            },
                        },
                    };
                    let inputs = [input(MOUSEEVENTF_LEFTDOWN), input(MOUSEEVENTF_LEFTUP)];
                    let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
                    if sent != inputs.len() as u32 {
                        if sent == 1 {
                            let _ = SendInput(
                                &[input(MOUSEEVENTF_LEFTUP)],
                                std::mem::size_of::<INPUT>() as i32,
                            );
                        }
                        return Err("receiver click was not fully sent".into());
                    }
                    self.command(json!({"command": "read"}))?;
                    self.activate(0)?;
                    Ok(())
                })();
                let mut current = POINT::default();
                if GetCursorPos(&mut current).is_ok()
                    && current.x == point.x
                    && current.y == point.y
                {
                    let _ = SetCursorPos(previous.x, previous.y);
                }
                clicked?;
            }
            Ok(())
        }

        fn clear(&mut self, index: usize) -> Result<()> {
            self.command(json!({"command": "clear", "target": index}))?;
            Ok(())
        }

        fn text(&mut self, index: usize) -> Result<String> {
            Ok(
                self.command(json!({"command": "read", "target": index}))?["text"]
                    .as_str()
                    .ok_or("missing receiver text")?
                    .to_string(),
            )
        }

        fn expect_text(&mut self, index: usize, expected: &str) -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let received = self.text(index)?;
                if received == expected {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    let diagnostics = self.command(json!({"command": "read", "target": index}))?;
                    return Err(format!(
                        "receiver {index}: expected {expected:?}, received {received:?}; diagnostics={diagnostics}; clipboard={:?}", clipboard::try_read_text()
                    )
                    .into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for ReceiverProcess {
        fn drop(&mut self) {
            unsafe {
                let mut pid = 0;
                GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
                if pid == self.child.id() && self.previous_foreground != 0 {
                    let _ = SetForegroundWindow(hwnd(self.previous_foreground));
                }
            }
            let _ = writeln!(self.input, "{}", json!({"command": "quit"}));
            let _ = self.input.flush();
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn passed(checks: &mut Vec<&'static str>, name: &'static str) {
        checks.push(name);
        println!("PASS: {name}");
    }

    fn run(root: PathBuf) -> Result<()> {
        std::fs::create_dir_all(&root)?;
        // Initialize an isolated backend without hooks, monitoring or startup
        // integration. Then enable real clipboard/input only for this process.
        clipboard::force_headless(true);
        let host = HostState::open(AppPaths::from_data_root(root.join("data")), 28965)?;
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "0");
        clipboard::force_headless(false);
        assert!(!clipboard::is_headless());
        unsafe {
            for key in [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN] {
                if GetAsyncKeyState(key.0 as i32) < 0 {
                    return Err(
                        "release modifier keys before running native input verification".into(),
                    );
                }
            }
        }
        let mut fixture = ReceiverProcess::start()?;
        fixture
            .initial_focus()
            .map_err(|error| format!("initial receiver focus unavailable: {error}"))?;
        let mut checks = Vec::new();
        let text = "中文 🦀 native paste\r\n第二行\tTab";
        let created = host.add_text(&json!({"text": "original"}))?;
        let id = created["createdIds"][0].as_str().ok_or("no created clip")?;
        host.patch_item(&json!({"id": id, "bodyText": text}))?;
        let session = host.create_session();
        let found = host.search_items(
            &json!({"webSessionId": session["sessionId"], "query": "native paste"}),
        )?;
        assert_eq!(found["items"][0]["id"], id);

        let target = fixture.activate(0)?;
        host.paste_item(&json!({"id": id}))?;
        fixture.expect_text(0, text)?;
        passed(
            &mut checks,
            "edited history text reaches native EDIT through Ctrl+V",
        );

        paste::send_enter_after_to(40, Some(target))?;
        fixture.expect_text(0, &format!("{text}\r\n"))?;
        passed(&mut checks, "paste followed by Enter");

        fixture.clear(0)?;
        let favor = host.favor_add_text_items(&json!({"text": text}))?;
        let favor_id = favor["createdIds"][0]
            .as_str()
            .ok_or("no created favorite")?;
        host.favor_paste(&json!({"id": favor_id}))?;
        fixture.expect_text(0, text)?;
        passed(&mut checks, "favorite text reaches native EDIT");

        fixture.clear(0)?;
        let executable = std::env::current_exe()?;
        let opts = paste::PasteOptions {
            expected_target: Some(target),
            extra_shift_insert: executable
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            ..Default::default()
        };
        paste::paste_record_with(&ClipRecord::new_text(text, "native-test"), &opts)?;
        fixture.expect_text(0, text)?;
        passed(&mut checks, "Shift+Insert compatibility path");

        fixture.clear(0)?;
        paste::simulate_text_to("甲😀\r\n乙\tX", 5, Some(target))?;
        fixture.expect_text(0, "甲😀\r\n乙\tX")?;
        passed(
            &mut checks,
            "Unicode simulation with emoji, newline and Tab",
        );

        fixture.command(json!({"command": "maximize"}))?;
        fixture.activate(1)?;
        fixture.restore_target(target, "maximized")?;
        assert_eq!(
            fixture.command(json!({"command": "read"}))?["maximized"],
            true
        );
        fixture.command(json!({"command": "minimize"}))?;
        fixture.restore_target(target, "minimized")?;
        assert_eq!(
            fixture.command(json!({"command": "read"}))?["minimized"],
            false
        );
        fixture.command(json!({"command": "normal"}))?;
        passed(
            &mut checks,
            "focus restoration preserves maximized state and restores minimized targets",
        );

        fixture.clear(0)?;
        fixture.clear(1)?;
        fixture.activate(0)?;
        fixture.command(json!({"command": "switchAfterDelay", "milliseconds": 60}))?;
        let delayed = paste::PasteOptions {
            expected_target: Some(target),
            delay_ms: 250,
            ..Default::default()
        };
        assert!(paste::send_paste(&delayed).is_err());
        fixture.expect_text(0, "")?;
        fixture.expect_text(1, "")?;
        assert!(paste::send_enter_after_to(0, Some(target)).is_err());
        passed(
            &mut checks,
            "delayed paste and Enter stop after focus changes",
        );

        fixture.activate(0)?;
        fixture.command(json!({"command": "switchAfterChars", "count": 3}))?;
        assert!(paste::simulate_text_to("abcdefghi", 20, Some(target)).is_err());
        fixture.expect_text(0, "abc")?;
        fixture.expect_text(1, "")?;
        passed(
            &mut checks,
            "simulation stops after receiver switches focus on its third character",
        );
        let report = json!({"ok": true, "checks": checks, "receiverPid": fixture.child.id(), "dataRoot": root.join("data")});
        std::fs::write(
            root.join("report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        println!("{report}");
        Ok(())
    }

    fn require_released_keys() -> Result<()> {
        if (1..=255).any(|key| unsafe { GetAsyncKeyState(key) < 0 }) {
            return Err("desktop input is busy; no test keys sent".into());
        }
        Ok(())
    }

    fn hook_input(root: PathBuf) -> Result<()> {
        let root = root.canonicalize()?;
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors().nth(5).ok_or("workspace root unavailable")?.to_path_buf();
        let scratch = workspace.join(".tmp").canonicalize()?;
        if root == scratch || !root.starts_with(&scratch) {
            return Err("hook-input data must be inside workspace .tmp".into());
        }
        clipboard::force_headless(true);
        let host = HostState::open(AppPaths::from_data_root(root.join("data")), 32123)?;
        host.set_window_runtime_ready(true);
        host.set_clipboard_web_view_ready(true);
        host.request_show();
        host.apply_window_action_for("setPinned", true, "clipboard");
        while host.take_chrome_command().is_some() {}
        require_released_keys()?;
        let clipboard_before = unsafe { GetClipboardSequenceNumber() };
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "0");
        clipboard::force_headless(false);
        let mut fixture = ReceiverProcess::start()?;
        fixture.initial_focus()?;
        let target = fixture.activate(0)?;
        let _hooks = InputHooks::install(host.clone());
        let mut cases = Vec::new();
        for panel in [false, true] {
            if panel {
                host.favor_folder_create(&json!({"title": "Hook input fixture"}))?;
                host.panel_show(&json!({}))?;
                host.set_panel_web_view_ready(true);
                host.apply_window_action_for("setPinned", true, "panel");
            }
            while host.take_chrome_command().is_some() {}
            fixture.clear(0)?;
            target.verify()?;
            require_released_keys()?;
            // Positive control: external unmarked ArrowDown must enter the
            // real hook and queue navigation, rather than reach the receiver.
            // It cannot execute a paste, and no UI command consumer is started.
            use windows::Win32::UI::Input::KeyboardAndMouse::{
                INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VIRTUAL_KEY,
            };
            std::thread::sleep(Duration::from_millis(100));
            target.verify()?;
            require_released_keys()?;
            let key = |up| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 { ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0x28),
                    dwFlags: KEYEVENTF_EXTENDEDKEY | if up { KEYEVENTF_KEYUP } else { Default::default() },
                    ..Default::default()
                } },
            };
            let sent = unsafe { SendInput(&[key(false), key(true)], std::mem::size_of::<INPUT>() as i32) };
            if sent != 2 {
                if sent == 1 { unsafe { SendInput(&[key(true)], std::mem::size_of::<INPUT>() as i32); } }
                return Err("hook control input was incomplete".into());
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            let navigation = loop {
                if let Some(command) = host.take_chrome_command() { break command; }
                if Instant::now() >= deadline { return Err("real keyboard hook did not route the control key".into()); }
                std::thread::sleep(Duration::from_millis(5));
            };
            let expected = if panel { ChromeCommand::PanelNavKey("ArrowDown") }
                else { ChromeCommand::ClipboardNavKey { key: "ArrowDown", shift: false } };
            assert_eq!(navigation, expected);
            let control = fixture.command(json!({"command": "read"}))?;
            assert!(!control["keyboard"].as_array().unwrap().iter()
                .any(|event| event["message"] == WM_KEYDOWN && event["key"] == 0x28));
            require_released_keys()?;
            paste::simulate_text_to("甲😀\tX", 0, Some(target))?;
            paste::send_backspace_to(1, Some(target))?;
            paste::send_enter_after_to(0, Some(target))?;
            let expected_text = "甲😀\t\r\n";
            let deadline = Instant::now() + Duration::from_secs(2);
            let delivered = loop {
                let state = fixture.command(json!({"command": "read"}))?;
                if state["text"] == expected_text { break state; }
                if Instant::now() >= deadline {
                    return Err(format!("generated input failed: panel={panel}, receiver={state}").into());
                }
                target.verify()?;
                std::thread::sleep(Duration::from_millis(5));
            };
            assert!(host.take_chrome_command().is_none(), "generated input operated the host list");
            assert!(host.hide_snapshot().visible && host.hide_snapshot().has_fake_focus);
            assert_eq!(delivered["foreground"], fixture.windows[0]);
            cases.push(json!({"panel": panel, "controlNavigation": format!("{navigation:?}"),
                "receiver": delivered, "hostQueueEmpty": true}));
        }
        target.verify()?;
        unsafe {
            if fixture.previous_foreground != 0 {
                let _ = SetForegroundWindow(hwnd(fixture.previous_foreground));
            }
        }
        fixture.command(json!({"command": "quit"}))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(status) = fixture.child.try_wait()? { break status; }
            if Instant::now() >= deadline { return Err("receiver did not exit normally".into()); }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success());
        let clipboard_after = unsafe { GetClipboardSequenceNumber() };
        assert_eq!(clipboard_before, clipboard_after);
        let report = json!({"ok": true, "cases": cases, "receiverPid": fixture.child.id(),
            "receiverExitCode": status.code(), "clipboardBefore": clipboard_before,
            "clipboardAfter": clipboard_after, "clipboardWrites": false, "realInputSent": true,
            "scope": "Actual core InputHooks and production Unicode/chord/Enter senders; no Tauri WebView or clipboard-paste acceptance."});
        std::fs::write(root.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
        println!("{report}");
        Ok(())
    }

    unsafe extern "system" fn click_host_proc(window: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match msg {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_LBUTTONDOWN => { HOST_MOUSE_DOWN[0].fetch_add(1, Ordering::Relaxed); }
            WM_RBUTTONDOWN => { HOST_MOUSE_DOWN[1].fetch_add(1, Ordering::Relaxed); }
            WM_LBUTTONUP => { HOST_MOUSE_UP[0].fetch_add(1, Ordering::Relaxed); }
            WM_RBUTTONUP => { HOST_MOUSE_UP[1].fetch_add(1, Ordering::Relaxed); }
            WM_DESTROY => { PostQuitMessage(0); return LRESULT(0); }
            _ => {}
        }
        DefWindowProcW(window, msg, wp, lp)
    }

    // The owning thread pumps throughout receiver waits and hook callbacks.
    // This fixture has no WebView and consumes no queued page commands.
    struct ClickHostWindow {
        handle: isize,
        thread: Option<std::thread::JoinHandle<std::result::Result<(), String>>>,
    }

    impl ClickHostWindow {
        fn start(host: HostState) -> Result<Self> {
            let (send, receive) = mpsc::channel();
            let thread = std::thread::spawn(move || -> std::result::Result<(), String> {
                unsafe {
                    let module = GetModuleHandleW(None).map_err(|e| e.to_string())?;
                    let class = WNDCLASSW { lpfnWndProc: Some(click_host_proc),
                        hInstance: module.into(), lpszClassName: w!("ClipHostNoActivateClickFixture"),
                        ..Default::default() };
                    if RegisterClassW(&class) == 0 { return Err("cannot register click host class".into()); }
                    let mut work = RECT::default();
                    SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as *mut _),
                        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS::default()).map_err(|e| e.to_string())?;
                    let window = CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                        class.lpszClassName, w!("ClipHost no-activate click fixture"), WS_POPUP,
                        work.right - 310, work.top + 80, 280, 180, None, None, module, None)
                        .map_err(|e| e.to_string())?;
                    host.set_host_hwnd(window.0 as isize);
                    cliphost_core::window_chrome::install_html_titlebar_passthrough(window.0 as isize);
                    let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
                    if send.send(window.0 as isize).is_err() {
                        let _ = DestroyWindow(window);
                        return Err("click host driver stopped".into());
                    }
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    host.set_host_hwnd(0);
                    if IsWindow(window).as_bool() { let _ = DestroyWindow(window); }
                    if msg.wParam.0 != 0 { return Err("click host message loop failed".into()); }
                }
                Ok(())
            });
            let handle = receive.recv_timeout(Duration::from_secs(5))?;
            Ok(Self { handle, thread: Some(thread) })
        }

        fn close(&mut self) -> Result<()> {
            if let Some(thread) = self.thread.take() {
                unsafe { PostMessageW(hwnd(self.handle), WM_CLOSE, WPARAM(0), LPARAM(0))?; }
                thread.join().map_err(|_| "click host thread panicked")?.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
                assert!(!unsafe { IsWindow(hwnd(self.handle)).as_bool() });
            }
            Ok(())
        }
    }

    impl Drop for ClickHostWindow {
        fn drop(&mut self) { let _ = self.close(); }
    }

    struct CursorRestore { previous: POINT, last: POINT }
    impl CursorRestore {
        fn new() -> Result<Self> {
            let mut point = POINT::default();
            unsafe { GetCursorPos(&mut point)?; }
            Ok(Self { previous: point, last: point })
        }
        fn move_to(&mut self, point: POINT) -> Result<()> {
            let mut current = POINT::default();
            unsafe { GetCursorPos(&mut current)?; }
            if current.x != self.last.x || current.y != self.last.y {
                return Err("user moved the cursor; stopping fixture input".into());
            }
            require_released_keys()?;
            unsafe { SetCursorPos(point.x, point.y)?; }
            self.last = point;
            Ok(())
        }
    }
    impl Drop for CursorRestore {
        fn drop(&mut self) {
            let mut current = POINT::default();
            unsafe {
                if GetCursorPos(&mut current).is_ok() && current.x == self.last.x && current.y == self.last.y {
                    let _ = SetCursorPos(self.previous.x, self.previous.y);
                }
            }
        }
    }

    fn owned_click(window: isize, point: POINT, right: bool, target: InputTarget,
        cursor: &mut CursorRestore) -> Result<()> {
        target.verify()?;
        cursor.move_to(point)?;
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd(window), Some(&mut pid));
            if ![std::process::id(), DRIVER_PID.load(Ordering::Relaxed)].contains(&pid)
                || GetAncestor(WindowFromPoint(point), GA_ROOT) != hwnd(window) {
                return Err("click point is not an owned fixture window".into());
            }
            target.verify()?;
            require_released_keys()?;
            let (down, up) = if right { (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP) }
                else { (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP) };
            let input = |flags| INPUT { r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 { mi: MOUSEINPUT { dwFlags: flags, ..Default::default() } } };
            let sent = SendInput(&[input(down), input(up)], std::mem::size_of::<INPUT>() as i32);
            if sent != 2 {
                if sent == 1 { SendInput(&[input(up)], std::mem::size_of::<INPUT>() as i32); }
                return Err("fixture click was incomplete".into());
            }
        }
        // SendInput queues the pair asynchronously. Wait for its button-up
        // before the next pointer move; never force-release a physical key.
        await_fixture_input_release()?;
        Ok(())
    }

    fn await_fixture_input_release() -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(150);
        loop {
            if require_released_keys().is_ok() { return Ok(()); }
            if Instant::now() >= deadline { return Err("desktop input remains held; fixture stopped".into()); }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn hook_click(root: PathBuf) -> Result<()> {
        let root = root.canonicalize()?;
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors().nth(5).ok_or("workspace root unavailable")?.to_path_buf();
        let scratch = workspace.join(".tmp").canonicalize()?;
        if root == scratch || !root.starts_with(&scratch) {
            return Err("hook-click data must be inside workspace .tmp".into());
        }
        // A locked/secure desktop cannot be a native-input acceptance target.
        // Refuse before opening state, launching a receiver or installing hooks.
        let mut initial_cursor = POINT::default();
        unsafe {
            GetCursorPos(&mut initial_cursor)
                .map_err(|error| format!("input desktop unavailable before fixture startup: {error}"))?;
            if GetForegroundWindow().0.is_null() {
                return Err("input desktop has no foreground window; fixture was not started".into());
            }
        }
        clipboard::force_headless(true);
        let host = HostState::open(AppPaths::from_data_root(root.join("data")), 32123)?;
        host.set_window_runtime_ready(true);
        host.set_clipboard_web_view_ready(true);
        host.request_show();
        host.apply_window_action_for("setPinned", true, "clipboard");
        while host.take_chrome_command().is_some() {}
        require_released_keys()?;
        let clipboard_before = unsafe { GetClipboardSequenceNumber() };
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "0");
        clipboard::force_headless(false);
        let mut receiver = ReceiverProcess::start()?;
        // Store the only other allowed click PID; the child has its own statics.
        DRIVER_PID.store(receiver.child.id(), Ordering::Relaxed);
        receiver.initial_focus()?;
        await_fixture_input_release()?;
        let target = receiver.activate(0)?;
        eprintln!("Click fixture receiver ready: pid={}", receiver.child.id());
        let mut cursor = CursorRestore::new()?;
        let mut window = ClickHostWindow::start(host.clone())?;
        target.verify()?;
        let _hooks = InputHooks::install(host.clone());
        std::thread::sleep(Duration::from_millis(100));
        let no_activate = unsafe { GetWindowLongPtrW(hwnd(window.handle), GWL_EXSTYLE) as u32 & WS_EX_NOACTIVATE.0 != 0 };
        assert!(no_activate);
        let mut rect = RECT::default();
        unsafe { GetWindowRect(hwnd(window.handle), &mut rect)?; }
        let inside = POINT { x: rect.left + 80, y: rect.top + 80 };
        let mut receiver_client = RECT::default();
        unsafe { GetClientRect(hwnd(receiver.windows[0]), &mut receiver_client)?; }
        // Use the frame's empty right margin, outside EDIT: the receiver's
        // built-in edit context menu is unrelated to this acceptance test.
        let mut outside = POINT { x: receiver_client.right - 4, y: 30 };
        assert!(unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(hwnd(receiver.windows[0]), &mut outside).as_bool() });
        assert_eq!(unsafe { WindowFromPoint(outside) }, hwnd(receiver.windows[0]));
        cursor.move_to(outside)?;
        let mut cases = Vec::new();
        for right in [false, true] {
            receiver.clear(0)?;
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                host.notify_deactivated();
                if !host.hide_snapshot().has_fake_focus { break; }
                if Instant::now() >= deadline { return Err("fake focus did not release".into()); }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(host.hide_snapshot().visible && host.hide_snapshot().no_focus_session_active);
            while host.take_chrome_command().is_some() {}
            let index = usize::from(right);
            let received_before = HOST_MOUSE_DOWN[index].load(Ordering::Relaxed);
            let released_before = HOST_MOUSE_UP[index].load(Ordering::Relaxed);
            owned_click(window.handle, inside, right, target, &mut cursor)?;
            let deadline = Instant::now() + Duration::from_secs(1);
            while !host.hide_snapshot().has_fake_focus
                || HOST_MOUSE_DOWN[index].load(Ordering::Relaxed) == received_before
                || HOST_MOUSE_UP[index].load(Ordering::Relaxed) == released_before {
                target.verify()?;
                if Instant::now() >= deadline { return Err("native host click did not restore fake focus and reach its window".into()); }
                std::thread::sleep(Duration::from_millis(5));
            }
            target.verify()?;
            cursor.move_to(outside)?;
            host.poll_hide();
            let snap = host.hide_snapshot();
            assert!(snap.has_fake_focus && snap.visible && !snap.pointer_in_host);
            assert!(host.take_chrome_command().is_none());
            use windows::Win32::UI::Input::KeyboardAndMouse::{
                INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VIRTUAL_KEY,
            };
            require_released_keys()?;
            target.verify()?;
            let key = |up| INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0x28),
                dwFlags: KEYEVENTF_EXTENDEDKEY | if up { KEYEVENTF_KEYUP } else { Default::default() },
                ..Default::default()
            } } };
            let sent = unsafe { SendInput(&[key(false), key(true)], std::mem::size_of::<INPUT>() as i32) };
            if sent != 2 {
                if sent == 1 { unsafe { SendInput(&[key(true)], std::mem::size_of::<INPUT>() as i32); } }
                return Err("navigation control input was incomplete".into());
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            let navigation = loop {
                if let Some(command) = host.take_chrome_command() { break command; }
                if Instant::now() >= deadline { return Err("navigation after native click did not route".into()); }
                std::thread::sleep(Duration::from_millis(5));
            };
            assert_eq!(navigation, ChromeCommand::ClipboardNavKey { key: "ArrowDown", shift: false });
            // External right-click stays within the dedicated receiver and must
            // neither dismiss this pinned clipboard nor steal its input focus.
            owned_click(receiver.windows[0], outside, true, target, &mut cursor)?;
            let state = receiver.command(json!({"command": "read"}))?;
            target.verify()?;
            assert_eq!(state["foreground"], receiver.windows[0]);
            assert_eq!(state["focus"], state["edits"][0]);
            assert_eq!(state["text"], "");
            assert!(!state["keyboard"].as_array().unwrap().iter().any(|event|
                event["message"] == WM_KEYDOWN && event["key"] == 0x28));
            assert!(host.hide_snapshot().visible && host.hide_snapshot().has_fake_focus);
            assert!(host.take_chrome_command().is_none());
            cases.push(json!({"button": if right { "right" } else { "left" },
                "hostHwnd": window.handle, "point": [inside.x, inside.y],
                "nativeMouseDownDelta": HOST_MOUSE_DOWN[index].load(Ordering::Relaxed) - received_before,
                "nativeMouseUpDelta": HOST_MOUSE_UP[index].load(Ordering::Relaxed) - released_before,
                "fakeFocusRestored": true, "pointerLeft": true, "outsideRightPreserved": true,
                "navigation": format!("{navigation:?}"), "receiver": state}));
        }
        window.close()?;
        drop(cursor);
        target.verify()?;
        unsafe {
            if receiver.previous_foreground != 0 { let _ = SetForegroundWindow(hwnd(receiver.previous_foreground)); }
        }
        receiver.command(json!({"command": "quit"}))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(status) = receiver.child.try_wait()? { break status; }
            if Instant::now() >= deadline { return Err("receiver did not exit normally".into()); }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success());
        let clipboard_after = unsafe { GetClipboardSequenceNumber() };
        assert_eq!(clipboard_before, clipboard_after);
        let report = json!({"ok": true, "cases": cases, "receiverPid": receiver.child.id(),
            "receiverExitCode": status.code(), "hostThreadExited": true,
            "hostNoActivate": no_activate,
            "clipboardBefore": clipboard_before, "clipboardAfter": clipboard_after,
            "clipboardWrites": false, "realInputSent": true,
            "scope": "Actual core mouse and keyboard hooks with a dedicated WS_EX_NOACTIVATE host and foreground Win32 receiver; no Tauri WebView or edge-hover acceptance."});
        std::fs::write(root.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
        println!("{report}");
        Ok(())
    }

    pub fn main() -> Result<()> {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let args: Vec<_> = std::env::args().collect();
        match args.get(1).map(String::as_str) {
            Some("--receiver") => receiver(args.get(2).ok_or("driver PID is required")?.parse()?),
            Some(mode @ ("--run" | "--hook-input" | "--hook-click")) => {
                let root = PathBuf::from(args.get(2).ok_or("temporary root is required")?);
                std::fs::create_dir_all(&root)?;
                let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    match mode {
                        "--hook-input" => hook_input(root.clone()),
                        "--hook-click" => hook_click(root.clone()),
                        _ => run(root.clone()),
                    }
                })) {
                    Ok(result) => result,
                    Err(error) => {
                        let message = error
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| error.downcast_ref::<&str>().copied())
                            .unwrap_or("native input assertion failed");
                        Err(message.to_string().into())
                    }
                };
                if let Err(error) = &result {
                    let report = json!({"ok": false, "error": error.to_string(), "dataRoot": root.join("data")});
                    std::fs::write(
                        root.join("report.json"),
                        serde_json::to_vec_pretty(&report)?,
                    )?;
                }
                result
            }
            _ => {
                println!("Run explicitly with --run, --hook-input or --hook-click <temporary-root>; these modes open dedicated windows and send real input.");
                Ok(())
            }
        }
    }
}
