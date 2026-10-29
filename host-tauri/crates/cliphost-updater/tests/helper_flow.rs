#![cfg(all(windows, feature = "test-support"))]
use cliphost_updater::{test_support, OfficialRelease};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

mod common;

// Retain native handles for only the fixture images inside this case's unique
// sandbox. PID reuse cannot redirect forced fallback cleanup to another app.
struct FixtureProcess(windows::Win32::Foundation::HANDLE, u32);

impl FixtureProcess {
    fn open(pid: u32, expected: &Path, sandbox: &Path) -> Option<Self> {
        use windows::{core::PWSTR, Win32::System::Threading::*};
        let expected = fs::canonicalize(expected).ok()?;
        if !expected.starts_with(fs::canonicalize(sandbox).ok()?) {
            return None;
        }
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                false,
                pid,
            )
        }
        .ok()?;
        let process = Self(handle, pid);
        let mut path = vec![0_u16; 32768];
        let mut length = path.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut length,
            )
        }
        .ok()?;
        let actual = fs::canonicalize(PathBuf::from(String::from_utf16_lossy(
            &path[..length as usize],
        )))
        .ok()?;
        actual
            .to_string_lossy()
            .eq_ignore_ascii_case(&expected.to_string_lossy())
            .then_some(process)
    }
    fn exited(&self) -> bool {
        unsafe {
            windows::Win32::System::Threading::WaitForSingleObject(self.0, 0)
                == windows::Win32::Foundation::WAIT_OBJECT_0
        }
    }
    fn stop(&self) {
        unsafe {
            let _ = windows::Win32::System::Threading::TerminateProcess(self.0, 70);
        }
    }
}
impl Drop for FixtureProcess {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

struct OwnedFixtures {
    parent: Child,
    sandbox: PathBuf,
    stage: PathBuf,
    data: PathBuf,
    control: PathBuf,
    report: Option<PathBuf>,
}

impl Drop for OwnedFixtures {
    fn drop(&mut self) {
        let _ = fs::write(self.data.join("stop"), "owned fixture cleanup");
        let _ = fs::write(&self.control, "stop");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut processes: Vec<FixtureProcess> = Vec::new();
        let mut settled = None;
        loop {
            let plan = fs::read(self.stage.join("plan.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            if let Ok(entries) = fs::read_dir(&self.stage) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if let Some(session) = name
                        .strip_prefix("helper-process-")
                        .and_then(|n| n.strip_suffix(".json"))
                    {
                        if uuid::Uuid::parse_str(session).is_err() {
                            continue;
                        }
                        if let Some(token) = plan.as_ref().and_then(|p| p["token"].as_str()) {
                            let _ =
                                fs::write(self.stage.join(format!("cancel-{session}.json")), token);
                        }
                        if let Some(pid) = fs::read(entry.path())
                            .ok()
                            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                            .and_then(|v| v["pid"].as_u64())
                        {
                            if !processes.iter().any(|p| p.1 == pid as u32) {
                                if let Some(process) = FixtureProcess::open(
                                    pid as u32,
                                    &self.stage.join("helper/cliphost-updater.exe"),
                                    &self.sandbox,
                                ) {
                                    processes.push(process);
                                }
                            }
                        }
                    }
                }
            }
            if let Some(pid) = fs::read(self.data.join("restart-argv.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| v["pid"].as_u64())
            {
                if !processes.iter().any(|p| p.1 == pid as u32) {
                    if let Some(process) = FixtureProcess::open(
                        pid as u32,
                        &self.sandbox.join("program/cliphost-app.exe"),
                        &self.sandbox,
                    ) {
                        processes.push(process);
                    }
                }
            }
            if self.parent.try_wait().ok().flatten().is_some()
                && processes.iter().all(FixtureProcess::exited)
            {
                let since = settled.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_millis(500) {
                    break;
                }
            } else {
                settled = None;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let mut forced = Vec::new();
        if self.parent.try_wait().ok().flatten().is_none() {
            forced.push(self.parent.id());
            let _ = self.parent.kill();
        }
        for process in &processes {
            if !process.exited() {
                forced.push(process.1);
                process.stop();
            }
        }
        let finish = Instant::now() + Duration::from_secs(1);
        while Instant::now() < finish
            && (self.parent.try_wait().ok().flatten().is_none()
                || processes.iter().any(|p| !p.exited()))
        {
            thread::sleep(Duration::from_millis(20));
        }
        if let Some(report) = &self.report {
            let _ = fs::create_dir_all(report);
            if let Ok(bytes) = serde_json::to_vec_pretty(
                &json!({"onlyOwnedSandboxProcesses":true,"forcedFixturePids":forced,"parentExited":self.parent.try_wait().ok().flatten().is_some(),"trackedFixtureProcessesExited":processes.iter().all(FixtureProcess::exited)}),
            ) {
                let _ = fs::write(report.join("cleanup.json"), bytes);
            }
        }
    }
}

fn wait(mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while !condition() {
        assert!(Instant::now() < end, "isolated update fixture timed out");
        thread::sleep(Duration::from_millis(50));
    }
}

fn manifest(root: &Path, fixture: &[u8], runtime: &[u8], version: &str) {
    fs::create_dir_all(root).unwrap();
    let mut files = Vec::new();
    for (name, bytes) in [("cliphost-app.exe", fixture), ("vcruntime140.dll", runtime)] {
        fs::write(root.join(name), bytes).unwrap();
        files.push(json!({"name":name,"sha256":format!("{:x}",Sha256::digest(bytes)),"sizeBytes":bytes.len()}));
    }
    fs::write(root.join("build-info.json"), serde_json::to_vec_pretty(&json!({"product":"ClipHost Rust","version":version,"executable":"cliphost-app.exe", "sha256":files[0]["sha256"],"renderer":"gpui","profile":"release","architecture":"x64","target":"x86_64-pc-windows-msvc","files":files,"launcherVersion":"0.31.27.1","packageName":"cea.cliphost.rust","executableBuild":{"fixture":true}})).unwrap()).unwrap();
    fs::write(root.join("README.txt"), "synthetic fixture").unwrap();
    fs::write(root.join("SHA256SUMS.txt"), "synthetic fixture").unwrap();
}

#[test]
fn isolated_helper_authorization_cancel_retry_restart_and_failure_rollback() {
    let tests_root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/updater-helper-tests");
    fs::create_dir_all(&tests_root).unwrap();
    let fixture = fs::read(env!("CARGO_BIN_EXE_cliphost-update-fixture")).unwrap();
    let runtime = fs::read(
        PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/vcruntime140.dll"),
    )
    .unwrap();
    let report_root = common::report_directory();
    let mut reports = Vec::new();
    for mode in [
        "authorized",
        "unapproved",
        "cancel-retry",
        "restart-failure",
    ] {
        let temp = tempfile::Builder::new()
            .prefix(mode)
            .tempdir_in(&tests_root)
            .unwrap();
        let root = temp.path().join("program");
        manifest(&root, &fixture, &runtime, "1.0.0.0");
        let old_sha = format!("{:x}", Sha256::digest(&fixture));
        let stage = root
            .join(".cliphost-updates")
            .join(uuid::Uuid::new_v4().to_string());
        let mut newer = fixture.clone();
        newer.extend_from_slice(b"new synthetic release overlay\n");
        let mut download_progress = Vec::new();
        if mode == "authorized" {
            let source = temp.path().join("release-source");
            manifest(&source, &newer, &runtime, "2.0.0.0");
            fs::create_dir_all(&stage).unwrap();
            let archive = common::archive_directory(&source);
            let hash = format!("{:x}", Sha256::digest(&archive));
            let length = archive.len() as u64;
            let (address, server) = common::serve_once(archive, None, false);
            let release = OfficialRelease {
                version: "2.0.0.0".into(),
                download_urls: vec![address],
                sha256: hash,
                size: Some(length),
                release_url: "http://127.0.0.1/simulation-only".into(),
                release_notes: None,
                source: "isolated loopback simulation".into(),
            };
            test_support::download_loopback(&release, &stage.join("package.zip"), |done, total| {
                download_progress.push(json!({"downloadedBytes":done,"totalBytes":total}));
            })
            .unwrap();
            server.join().unwrap();
            assert_eq!(download_progress.last().unwrap()["downloadedBytes"], length);
            test_support::extract_downloaded(
                &stage.join("package.zip"),
                &stage.join("payload"),
                "2.0.0.0",
            )
            .unwrap();
        } else {
            manifest(&stage.join("payload"), &newer, &runtime, "2.0.0.0");
        }
        let data = temp.path().join("original data 空格");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("userdata.db"), "original fixture database").unwrap();
        if mode == "restart-failure" {
            fs::write(data.join("fail-version"), "2.0.0.0").unwrap();
        }
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let ready = temp.path().join("parent-ready.json");
        let control = temp.path().join("control");
        let mut command = Command::new(root.join("cliphost-app.exe"));
        command
            .args(["--fixture-parent"])
            .arg(&stage)
            .arg(&ready)
            .arg(&control)
            .arg(&data)
            .arg(port.to_string());
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
        let mut owned = OwnedFixtures {
            parent: command.spawn().unwrap(),
            sandbox: temp.path().to_path_buf(),
            stage: stage.clone(),
            data: data.clone(),
            control: control.clone(),
            report: report_root.as_ref().map(|p| p.join(mode)),
        };
        let parent = &mut owned.parent;
        wait(|| ready.exists() || parent.try_wait().unwrap().is_some());
        assert!(
            ready.exists(),
            "fixture parent failed before helper ready: {mode}"
        );
        let mut session: String = serde_json::from_slice::<Value>(&fs::read(&ready).unwrap())
            .unwrap()["session"]
            .as_str()
            .unwrap()
            .into();
        if mode == "cancel-retry" {
            fs::write(&control, "cancel").unwrap();
            wait(|| stage.join(format!("result-{session}.json")).exists());
            assert_eq!(
                serde_json::from_slice::<Value>(&fs::read(root.join("build-info.json")).unwrap())
                    .unwrap()["version"],
                "1.0.0.0"
            );
            fs::write(&control, "retry").unwrap();
            wait(|| {
                fs::read(&ready)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .and_then(|v| v["session"].as_str().map(|s| s != session))
                    .unwrap_or(false)
            });
            session = serde_json::from_slice::<Value>(&fs::read(&ready).unwrap()).unwrap()
                ["session"]
                .as_str()
                .unwrap()
                .into();
        }
        fs::write(
            &control,
            if mode == "unapproved" {
                "exit-unapproved"
            } else {
                "authorize"
            },
        )
        .unwrap();
        wait(|| parent.try_wait().unwrap().is_some());
        let result_path = stage.join(format!("result-{session}.json"));
        wait(|| result_path.exists());
        let outcome: Value = serde_json::from_slice(&fs::read(&result_path).unwrap()).unwrap();
        let installed: Value =
            serde_json::from_slice(&fs::read(root.join("build-info.json")).unwrap()).unwrap();
        if matches!(mode, "authorized" | "cancel-retry") {
            assert_eq!(outcome["status"], "updated", "{outcome}");
            assert_eq!(installed["version"], "2.0.0.0");
            let argv: Value =
                serde_json::from_slice(&fs::read(data.join("restart-argv.json")).unwrap()).unwrap();
            assert_eq!(argv["dataEnv"], data.to_string_lossy().as_ref());
            assert_eq!(
                argv["args"],
                json!(["--data-root", data, "--api-port", port.to_string()])
            );
            assert_eq!(
                format!(
                    "{:x}",
                    Sha256::digest(fs::read(stage.join("rollback/cliphost-app.exe")).unwrap())
                ),
                old_sha
            );
            assert!(!stage.join("payload").exists());
        } else {
            assert_eq!(outcome["status"], "failed");
            assert_eq!(installed["version"], "1.0.0.0");
            assert_eq!(
                format!(
                    "{:x}",
                    Sha256::digest(fs::read(root.join("cliphost-app.exe")).unwrap())
                ),
                old_sha
            );
        }
        assert_eq!(
            fs::read_to_string(data.join("userdata.db")).unwrap(),
            "original fixture database"
        );
        let report = json!({"mode":mode,"passed":true,"outcome":outcome,"installedVersion":installed["version"],
            "installedExeSha256":format!("{:x}",Sha256::digest(fs::read(root.join("cliphost-app.exe")).unwrap())),
            "originalExeSha256":old_sha,"fixtureParentPid":parent.id(),"port":port,"dataPreserved":true,
            "downloadProgress":download_progress,"runtime":"synthetic host on loopback; no GPUI/clipboard", "source":"generated local test binaries; no release published"});
        if let Some(directory) = &report_root {
            let case = directory.join(mode);
            fs::create_dir_all(&case).unwrap();
            fs::write(
                case.join("receipt.json"),
                serde_json::to_vec_pretty(&report).unwrap(),
            )
            .unwrap();
            for name in ["plan.json", "update.log", "journal.jsonl"] {
                let path = stage.join(name);
                if path.exists() {
                    fs::copy(path, case.join(name)).unwrap();
                }
            }
            if data.join("restart-argv.json").exists() {
                fs::copy(
                    data.join("restart-argv.json"),
                    case.join("restart-argv.json"),
                )
                .unwrap();
            }
        }
        reports.push(report);
        drop(owned);
    }
    if let Some(directory) = &report_root {
        fs::write(
            directory.join("helper-scenarios.json"),
            serde_json::to_vec_pretty(&reports).unwrap(),
        )
        .unwrap();
    }
}
