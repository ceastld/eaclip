//! Additive explicit-target RPC. Old runVerb payloads and behavior are untouched.
use super::HostState;
use crate::invocation::{self, BoundVerb, TargetSnapshot};
use serde_json::{json, Value};
use std::{path::Path, sync::atomic::Ordering};

impl HostState {
    /// Script-test startup must not consult the inherited legacy QA parent or
    /// touch descendants before their fixed-tree/link checks. New roots remain
    /// valid: there is no existing child tree to inspect until startup creates it.
    pub fn validate_script_test_startup_root(root: &Path) -> Result<std::path::PathBuf, String> {
        #[cfg(not(any(debug_assertions, test)))]
        {
            let _ = root;
            Err("script_test_requires_debug".into())
        }
        #[cfg(any(debug_assertions, test))]
        {
            let root = invocation::checked_root(root)?;
            let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(3)
                .ok_or("script_test_workspace_boundary_unavailable")?;
            let allowed = invocation::checked_root(&workspace.join(".tmp"))?;
            if root == allowed || !root.starts_with(&allowed) {
                return Err("script_test_root_must_be_fixed_workspace_tmp_child".into());
            }
            match std::fs::symlink_metadata(&root) {
                Ok(_) => super::validate_isolated_core_copy_tree(
                    &crate::paths::AppPaths::from_data_root(root.clone()),
                )?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
            Ok(root)
        }
    }

    pub fn start_script_test_rpc(&self) -> Result<(), String> {
        let name = invocation::test_pipe_name(&self.data_root())?;
        self.verb_rpc_stop.reset();
        crate::pipe_rpc::spawn_listeners(
            self.clone(),
            &[&name],
            self.verb_rpc_stop.clone(),
            self.verb_rpc_listening.clone(),
        )?;
        if !self.verb_rpc_listening.load(Ordering::Acquire) {
            return Err(
                "Explicit script-test RPC was disabled; no shared endpoint fallback.".into(),
            );
        }
        Ok(())
    }

    pub(crate) fn invocation_info(&self) -> Result<Value, String> {
        Ok(json!({"schema_version":1, "target_context_version":1,
            "data_root":invocation::checked_root(&self.data_root())?,
            "test_profile":self.keyboard_input_qa,
            "capture_enabled":self.inner.lock().config.clip_monitor_on}))
    }

    pub(crate) fn run_bound_verb(&self, payload: &Value) -> Result<Value, String> {
        let request: BoundVerb =
            serde_json::from_value(payload.clone()).map_err(|error| error.to_string())?;
        execute_checked(
            request,
            &self.data_root(),
            std::process::id(),
            invocation::now_ms,
            verify_native,
            |command, context| self.execute_verb_with_context(command, context),
        )
    }
}

fn execute_checked(
    request: BoundVerb,
    actual_root: &Path,
    own_pid: u32,
    mut clock: impl FnMut() -> u64,
    mut verify: impl FnMut(&TargetSnapshot, u64) -> Result<(), String>,
    execute: impl FnOnce(&str, Option<&crate::verb::VerbRunContext>) -> Result<Value, String>,
) -> Result<Value, String> {
    if request.schema_version != 1
        || request.command.trim().is_empty()
        || request.command.len() > 64 * 1024
        || request.command.contains('\0')
    {
        return Err("invalid_bound_verb".into());
    }
    request.target.validate_at(clock())?;
    if !invocation::same_root(&request.expected_data_root, actual_root)? {
        return Err("invocation_data_root_mismatch".into());
    }
    verify(&request.target, clock())?;
    // Own-window toggle/hide must not replace the previously bound external
    // paste context with a host HWND. Other own-window commands stay explicit.
    let context = if request.target.process_id == own_pid {
        if !matches!(
            request.command.as_str(),
            "clip toggle" | "clip hide" | "clip close"
        ) {
            return Err("target_is_host_for_external_command".into());
        }
        None
    } else {
        Some(crate::verb::VerbRunContext {
            target_window_handle: request
                .target
                .window_handle
                .parse()
                .map_err(|_| "invalid_target")?,
            ..Default::default()
        })
    };
    verify(&request.target, clock())?;
    // Native verification may itself cross the expiry deadline.
    request.target.validate_at(clock())?;
    execute(&request.command, context.as_ref())?;
    Ok(
        json!({"accepted":true,"scope":"existing_verb_accepted","target_context_verified":true,
        "data_root":invocation::checked_root(actual_root)?}),
    )
}

#[cfg(windows)]
fn verify_native(target: &TargetSnapshot, now: u64) -> Result<(), String> {
    use windows::Win32::{
        Foundation::{CloseHandle, FILETIME, HWND},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId, IsWindow, IsWindowVisible,
        },
    };
    let (handle, created) = target.validate_at(now)?;
    if handle > isize::MAX as i64 {
        return Err("target_handle_outside_native_width".into());
    }
    unsafe {
        let hwnd = HWND(handle as isize as *mut _);
        let mut pid = 0;
        let tid = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if !IsWindow(hwnd).as_bool()
            || !IsWindowVisible(hwnd).as_bool()
            || pid != target.process_id
            || tid != target.thread_id
            || GetForegroundWindow() != hwnd
        {
            return Err("invocation_target_changed".into());
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(|error| error.to_string())?;
        let (mut creation, mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let result = GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user)
            .map_err(|error| error.to_string());
        let _ = CloseHandle(process);
        result?;
        if ((creation.dwHighDateTime as u64) << 32 | creation.dwLowDateTime as u64) != created {
            return Err("invocation_target_process_reused".into());
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn verify_native(_: &TargetSnapshot, _: u64) -> Result<(), String> {
    Err("explicit_target_requires_windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    fn request(root: &Path) -> BoundVerb {
        BoundVerb {
            schema_version: 1,
            expected_data_root: root.into(),
            command: "clip toggle".into(),
            target: TargetSnapshot {
                window_handle: "9007199254740993".into(),
                process_id: 2,
                thread_id: 3,
                process_created_file_time: "18446744073709550000".into(),
                expires_at_unix_ms: 2000,
            },
        }
    }
    #[test]
    fn bound_rpc_reuses_real_verb_entry_and_checks_target_twice() {
        let dir = tempfile::tempdir().unwrap();
        let verified = Cell::new(0);
        let result = execute_checked(
            request(dir.path()),
            dir.path(),
            9,
            || 1000,
            |_, _| {
                verified.set(verified.get() + 1);
                Ok(())
            },
            |command, context| {
                assert_eq!(command, "clip toggle");
                assert_eq!(context.unwrap().target_window_handle, 9007199254740993);
                Ok(Value::Null)
            },
        )
        .unwrap();
        assert_eq!(verified.get(), 2);
        assert_eq!(result["accepted"], true);
    }
    #[test]
    fn bound_rpc_root_expiry_and_last_edge_rejection_never_execute() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let executed = Cell::new(0);
        let mut wrong = request(other.path());
        wrong.expected_data_root = other.path().into();
        assert!(execute_checked(
            wrong,
            dir.path(),
            9,
            || 1000,
            |_, _| Ok(()),
            |_, _| {
                executed.set(1);
                Ok(Value::Null)
            }
        )
        .is_err());
        assert!(execute_checked(
            request(dir.path()),
            dir.path(),
            9,
            || 2000,
            |_, _| Ok(()),
            |_, _| {
                executed.set(1);
                Ok(Value::Null)
            }
        )
        .is_err());
        let verified = Cell::new(0);
        assert!(execute_checked(
            request(dir.path()),
            dir.path(),
            9,
            || 1000,
            |_, _| {
                verified.set(verified.get() + 1);
                if verified.get() == 2 {
                    Err("changed".into())
                } else {
                    Ok(())
                }
            },
            |_, _| {
                executed.set(1);
                Ok(Value::Null)
            }
        )
        .is_err());
        assert_eq!(executed.get(), 0);
    }
    #[test]
    fn own_window_toggle_preserves_external_context() {
        let dir = tempfile::tempdir().unwrap();
        execute_checked(
            request(dir.path()),
            dir.path(),
            2,
            || 1000,
            |_, _| Ok(()),
            |_, context| {
                assert!(context.is_none());
                Ok(Value::Null)
            },
        )
        .unwrap();
    }
}

#[cfg(test)]
mod startup_root_tests {
    use super::*;
    use std::cell::Cell;

    fn request(root: &Path) -> BoundVerb {
        BoundVerb {
            schema_version: 1,
            expected_data_root: root.into(),
            command: "clip toggle".into(),
            target: TargetSnapshot {
                window_handle: "9007199254740993".into(),
                process_id: 2,
                thread_id: 3,
                process_created_file_time: "18446744073709550000".into(),
                expires_at_unix_ms: 2_000,
            },
        }
    }

    #[test]
    fn expires_during_final_native_verify_never_executes() {
        let root = tempfile::tempdir().unwrap();
        let now = Cell::new(1_000);
        let verified = Cell::new(0);
        let executed = Cell::new(false);
        let result = execute_checked(
            request(root.path()),
            root.path(),
            9,
            || now.get(),
            |_, observed_at| {
                assert_eq!(observed_at, 1_000);
                verified.set(verified.get() + 1);
                if verified.get() == 2 {
                    // 模拟最后一次Win32核验耗时；入口时间有效，返回时已到期。
                    now.set(2_000);
                }
                Ok(())
            },
            |_, _| {
                executed.set(true);
                Ok(Value::Null)
            },
        );
        assert_eq!(verified.get(), 2);
        assert!(result.is_err());
        assert!(!executed.get());
    }

    fn fixed_workspace() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
    }

    fn owned_parent() -> tempfile::TempDir {
        let parent = fixed_workspace().join(".tmp");
        std::fs::create_dir_all(&parent).unwrap();
        tempfile::Builder::new()
            .prefix("script-root-test-")
            .tempdir_in(parent)
            .unwrap()
    }

    #[test]
    fn fresh_fixed_root_is_allowed_without_creating_it() {
        let parent = owned_parent();
        let fresh = parent.path().join("fresh-profile");
        assert!(!fresh.exists());
        let checked = HostState::validate_script_test_startup_root(&fresh).unwrap();
        assert_eq!(checked, invocation::checked_root(&fresh).unwrap());
        assert!(!fresh.exists(), "preflight不得创建日志或数据目录");
    }

    #[test]
    fn existing_fixed_root_is_checked_and_outside_fixed_root_is_rejected() {
        let parent = owned_parent();
        let profile = parent.path().join("existing-profile");
        std::fs::create_dir(&profile).unwrap();
        assert!(HostState::validate_script_test_startup_root(&profile).is_ok());
        assert!(!profile.join("logs").exists());
        let outside = tempfile::Builder::new()
            .prefix("script-root-test-outside-")
            .tempdir_in(fixed_workspace())
            .unwrap();
        let fresh_outside = outside.path().join("fresh-profile");
        assert!(HostState::validate_script_test_startup_root(&fresh_outside).is_err());
        assert!(!fresh_outside.exists());
        assert!(
            HostState::validate_script_test_startup_root(&fixed_workspace().join(".tmp")).is_err()
        );
    }

    #[test]
    fn inherited_legacy_qa_root_is_ignored_in_an_isolated_child() {
        const CHILD: &str = "CLIPHOST_SCRIPT_ROOT_TEST_CHILD";
        if std::env::var(CHILD).as_deref() != Ok("1") {
            let parent = owned_parent();
            let missing_foreign_parent = parent.path().join("unrelated-parent-must-not-be-probed");
            let module = module_path!().split_once("::").unwrap().1;
            let exact =
                format!("{module}::inherited_legacy_qa_root_is_ignored_in_an_isolated_child");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &exact, "--nocapture"])
                .env(CHILD, "1")
                .env("CLIPHOST_GPUI_TEST_ROOT", &missing_foreign_parent)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("CLIPHOST_SCRIPT_ROOT_TEST_CHILD_CHECKED")
            );
            assert!(!missing_foreign_parent.exists());
            return;
        }
        let parent = owned_parent();
        let fresh = parent.path().join("fresh-profile");
        assert!(HostState::validate_script_test_startup_root(&fresh).is_ok());
        assert!(!fresh.exists());
        println!("CLIPHOST_SCRIPT_ROOT_TEST_CHILD_CHECKED");
    }
}
