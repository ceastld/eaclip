use crate::{
    invalid, lock,
    package::{
        self, hash_file, read_manifest, reject_links, validate_manifest, verify_payload, Manifest,
        Version, EXE, MANIFEST, METADATA,
    },
    platform, PreparedUpdate, Result, UpdateConfig,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const HELPER: &str = "cliphost-updater.exe";
const WAIT_EXIT: Duration = Duration::from_secs(180);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Plan {
    format: u32,
    token: String,
    stage: PathBuf,
    install_root: PathBuf,
    #[serde(default)]
    test_fixture: bool,
    executable: PathBuf,
    data_root: PathBuf,
    api_port: u16,
    parent_pid: u32,
    parent_created: u64,
    parent_sha256: String,
    old_manifest_sha256: String,
    new_manifest_sha256: String,
    old_version: String,
    new_version: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Outcome {
    status: String,
    message: String,
    version: String,
    log_path: PathBuf,
    time_unix: u64,
}

pub(crate) fn previous_result(root: &Path) -> Option<String> {
    let bytes =
        package::read_limited(&root.join(".cliphost-updates/last-result.json"), 64 * 1024).ok()?;
    let result: Outcome = serde_json::from_slice(&bytes).ok()?;
    Some(result.message)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    reject_links(path.parent().ok_or_else(|| invalid("文件路径无效"))?)?;
    let temporary = path.with_file_name(format!(".signal-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    let result = platform::publish_new_file(&temporary, path);
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub(crate) fn prepare(
    config: &UpdateConfig,
    stage: &Path,
    new_manifest: &Manifest,
) -> Result<PreparedUpdate> {
    prepare_inner(config, stage, new_manifest, false)
}

#[cfg(feature = "test-support")]
pub(crate) fn prepare_fixture(
    config: &UpdateConfig,
    stage: &Path,
    new_manifest: &Manifest,
) -> Result<PreparedUpdate> {
    prepare_inner(config, stage, new_manifest, true)
}

fn prepare_inner(
    config: &UpdateConfig,
    stage: &Path,
    new_manifest: &Manifest,
    test_fixture: bool,
) -> Result<PreparedUpdate> {
    let root = config
        .executable
        .parent()
        .ok_or_else(|| invalid("无法识别安装目录"))?;
    reject_links(root)?;
    reject_links(stage)?;
    let canonical_root = fs::canonicalize(root)?;
    let canonical_stage = fs::canonicalize(stage)?;
    let root = canonical_root.as_path();
    let stage = canonical_stage.as_path();
    if !config.data_root.is_absolute() {
        return Err(invalid("无法确认当前数据目录"));
    }
    let old = package::validate_installed(&config.executable, &config.current_version)?;
    supplement_launcher_contract(root, &stage.join("payload"))?;
    let self_process = platform::Process::open(std::process::id(), &config.executable, None)?;
    let helper_root = stage.join("helper");
    fs::create_dir(&helper_root)?;
    fs::copy(&config.executable, helper_root.join(HELPER))?;
    for file in &old.files {
        if file.name.to_ascii_lowercase().ends_with(".dll") {
            package::verify_file(root, file)?;
            fs::copy(root.join(&file.name), helper_root.join(&file.name))?;
            package::verify_file(&helper_root, file)?;
        }
    }
    let api_port = config.api_port;
    if matches!(api_port, 0 | 28865 | 18765 | 18776 | 28766) {
        return Err(invalid("当前API端口不能用于Rust更新"));
    }
    let plan = Plan {
        format: 1,
        token: uuid::Uuid::new_v4().to_string(),
        stage: stage.to_path_buf(),
        install_root: root.to_path_buf(),
        test_fixture,
        executable: root.join(EXE),
        data_root: config.data_root.clone(),
        api_port,
        parent_pid: std::process::id(),
        parent_created: self_process.creation_time()?,
        parent_sha256: hash_file(&config.executable)?,
        old_manifest_sha256: hash_file(&root.join(MANIFEST))?,
        new_manifest_sha256: hash_file(&stage.join("payload").join(MANIFEST))?,
        old_version: config.current_version.clone(),
        new_version: new_manifest.version.clone(),
    };
    let plan_path = stage.join("plan.json");
    write_new(&plan_path, &serde_json::to_vec_pretty(&plan)?)?;
    Ok(PreparedUpdate {
        version: plan.new_version,
        plan_path: plan_path.clone(),
        log_path: stage.join("update.log"),
        plan_sha256: hash_file(&plan_path)?,
        session: Arc::new(Mutex::new(None)),
    })
}

// Old portable releases omitted the Quicker compatibility contract. Preserve
// every original build field and every binary digest while retaining the
// installed floor. New releases declare their own floor at packaging time.
fn supplement_launcher_contract(installed: &Path, payload: &Path) -> Result<()> {
    let old: serde_json::Value = serde_json::from_slice(&package::read_limited(
        &installed.join(MANIFEST),
        1024 * 1024,
    )?)?;
    let mut new: serde_json::Value = serde_json::from_slice(&package::read_limited(
        &payload.join(MANIFEST),
        1024 * 1024,
    )?)?;
    let mut floor = "0.31.7.0".to_string();
    for manifest in [&old, &new] {
        if let Some(version) = manifest.get("launcherVersion").and_then(|v| v.as_str()) {
            if version.split('.').count() != 4 {
                return Err(invalid("启动器兼容版本必须为四段数字"));
            }
            if Version::parse(version)? > Version::parse(&floor)? {
                floor = version.into();
            }
        }
        if manifest
            .get("packageName")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s != "cea.cliphost.rust")
        {
            return Err(invalid("包名称不属于x64 Rust宿主"));
        }
    }
    new["packageName"] = "cea.cliphost.rust".into();
    new["launcherVersion"] = floor.into();
    let bytes = serde_json::to_vec_pretty(&new)?;
    let path = payload.join(MANIFEST);
    reject_links(&path)?;
    let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn read_plan(path: &Path, expected_sha: &str) -> Result<Plan> {
    if !package::valid_hash(expected_sha) || hash_file(path)? != expected_sha {
        return Err(invalid("更新计划已改变"));
    }
    let plan: Plan = serde_json::from_slice(&package::read_limited(path, 128 * 1024)?)?;
    if plan.format != 1
        || (plan.test_fixture && !cfg!(feature = "test-support"))
        || uuid::Uuid::parse_str(&plan.token).is_err()
        || !plan.data_root.is_absolute()
        || path != plan.stage.join("plan.json")
        || plan.executable != plan.install_root.join(EXE)
        || plan.stage.parent() != Some(plan.install_root.join(".cliphost-updates").as_path())
        || plan
            .stage
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| uuid::Uuid::parse_str(n).ok())
            .is_none()
        || matches!(plan.api_port, 0 | 28865 | 18765 | 18776 | 28766)
        || Version::parse(&plan.new_version)? <= Version::parse(&plan.old_version)?
    {
        return Err(invalid("更新计划路径或版本不符合约束"));
    }
    reject_links(&plan.install_root)?;
    reject_links(&plan.stage)?;
    Ok(plan)
}

fn marker(stage: &Path, kind: &str, session: &str) -> PathBuf {
    stage.join(format!("{kind}-{session}.json"))
}

pub(crate) fn signal(prepared: &PreparedUpdate, kind: &str) -> Result<()> {
    let plan = read_plan(&prepared.plan_path, &prepared.plan_sha256)?;
    let session = lock(&prepared.session)
        .clone()
        .ok_or_else(|| invalid("更新辅助进程尚未启动"))?;
    let path = marker(&plan.stage, kind, &session);
    if path.exists() {
        if package::read_limited(&path, 1024)? == plan.token.as_bytes() {
            return Ok(());
        }
        return Err(invalid("更新辅助进程信号不匹配"));
    }
    write_new(&path, plan.token.as_bytes())
}

fn signaled(plan: &Plan, kind: &str, session: &str) -> Result<bool> {
    let path = marker(&plan.stage, kind, session);
    if !path.exists() {
        return Ok(false);
    }
    if package::read_limited(&path, 1024)? != plan.token.as_bytes() {
        return Err(invalid("更新授权信号无效"));
    }
    Ok(true)
}

pub(crate) fn spawn(prepared: &PreparedUpdate) -> Result<()> {
    let plan = read_plan(&prepared.plan_path, &prepared.plan_sha256)?;
    if let Some(previous) = lock(&prepared.session).clone() {
        let cancel = marker(&plan.stage, "cancel", &previous);
        if !cancel.exists() {
            write_new(&cancel, plan.token.as_bytes())?;
        }
    }
    let session = uuid::Uuid::new_v4().to_string();
    *lock(&prepared.session) = Some(session.clone());
    let mut command = Command::new(plan.stage.join("helper").join(HELPER));
    command
        .arg("--apply-update")
        .arg(&prepared.plan_path)
        .arg("--plan-sha256")
        .arg(&prepared.plan_sha256)
        .arg("--update-session")
        .arg(&session)
        .current_dir(plan.stage.join("helper"));
    hide(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| invalid(format!("无法启动更新辅助进程，当前版本继续运行：{e}")))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if signaled(&plan, "ready", &session)? {
            return Ok(());
        }
        if child.try_wait()?.is_some() {
            let outcome =
                package::read_limited(&marker(&plan.stage, "result", &session), 64 * 1024)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Outcome>(&b).ok())
                    .map(|o| o.message)
                    .unwrap_or_else(|| "辅助进程提前退出".into());
            return Err(invalid(format!(
                "{outcome}；日志：{}",
                prepared.log_path.display()
            )));
        }
        if Instant::now() >= deadline {
            let _ = signal(prepared, "cancel");
            return Err(invalid("更新辅助进程准备超时，当前版本继续运行"));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn hide(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let _ = command;
}

/// Must run before GPUI, single-instance, data resolution and crash initialization.
/// An update invocation is always consumed, including malformed arguments.
pub fn maybe_apply_update_from_args() -> Option<Result<()>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if !args.iter().any(|a| a == "--apply-update") {
        return None;
    }
    Some((|| {
        if args.len() != 6
            || args[0] != "--apply-update"
            || args[2] != "--plan-sha256"
            || args[4] != "--update-session"
        {
            return Err(invalid("更新辅助进程参数无效"));
        }
        let sha = args[3]
            .to_str()
            .ok_or_else(|| invalid("更新摘要参数无效"))?;
        let session = args[5]
            .to_str()
            .ok_or_else(|| invalid("更新会话参数无效"))?;
        if uuid::Uuid::parse_str(session).is_err() {
            return Err(invalid("更新会话无效"));
        }
        let path = PathBuf::from(&args[1]);
        let plan = read_plan(&path, sha)?;
        let result = apply(&plan, session);
        let (status, message) = match &result {
            Ok(()) => ("updated", format!("已更新至 {}。", plan.new_version)),
            Err(error) => (
                "failed",
                format!(
                    "更新未完成：{error}；日志：{}",
                    plan.stage.join("update.log").display()
                ),
            ),
        };
        record_outcome(&plan, session, status, &message);
        result
    })())
}

fn log(plan: &Plan, message: &str) {
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(plan.stage.join("update.log"))
    {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = writeln!(file, "{now} {message}");
        let _ = file.sync_all();
    }
}

fn record_outcome(plan: &Plan, session: &str, status: &str, message: &str) {
    log(plan, message);
    let outcome = Outcome {
        status: status.into(),
        message: message.into(),
        version: plan.new_version.clone(),
        log_path: plan.stage.join("update.log"),
        time_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    if let Ok(bytes) = serde_json::to_vec_pretty(&outcome) {
        let _ = write_new(&marker(&plan.stage, "result", session), &bytes);
        let latest = plan.install_root.join(".cliphost-updates/last-result.json");
        let temporary = plan.stage.join(format!("last-result-{session}.json"));
        if write_new(&temporary, &bytes).is_ok() {
            if latest.exists() && reject_links(&latest).is_ok() {
                let _ = fs::remove_file(&latest);
            }
            let _ = fs::rename(temporary, latest);
        }
    }
}

fn validate_helper(plan: &Plan) -> Result<(Manifest, Manifest)> {
    let own = std::env::current_exe()?;
    let expected_helper = plan.stage.join("helper").join(HELPER);
    if !platform::same_path(&own, &expected_helper)? || hash_file(&own)? != plan.parent_sha256 {
        return Err(invalid("更新辅助程序身份不匹配"));
    }
    if hash_file(&plan.install_root.join(MANIFEST))? != plan.old_manifest_sha256
        || hash_file(&plan.executable)? != plan.parent_sha256
    {
        return Err(invalid("安装目录已由其它程序更新，请重新检查"));
    }
    let old = read_manifest(&plan.install_root)?;
    validate_manifest(&old, &plan.old_version, false)?;
    verify_payload(&plan.install_root, &old)?;
    for file in &old.files {
        if file.name.to_ascii_lowercase().ends_with(".dll") {
            package::verify_file(&plan.stage.join("helper"), file)?;
        }
    }
    let new = validate_staged(plan)?;
    Ok((old, new))
}

fn validate_staged(plan: &Plan) -> Result<Manifest> {
    let staged = plan.stage.join("payload");
    if hash_file(&staged.join(MANIFEST))? != plan.new_manifest_sha256 {
        return Err(invalid("已准备的更新清单发生改变"));
    }
    let new = read_manifest(&staged)?;
    validate_manifest(&new, &plan.new_version, true)?;
    verify_payload(&staged, &new)?;
    Ok(new)
}

fn apply(plan: &Plan, session: &str) -> Result<()> {
    log(
        plan,
        "Validating helper and acquiring Quicker startup/install gate",
    );
    let _gate = platform::InstallGate::acquire(&plan.install_root, plan.test_fixture)?;
    let (old, new) = validate_helper(plan)?;
    let parent =
        platform::Process::open(plan.parent_pid, &plan.executable, Some(plan.parent_created))?;
    if parent.exited()? {
        return Err(invalid("原程序在准备握手前已退出，更新未授权"));
    }
    write_new(
        &marker(&plan.stage, "ready", session),
        plan.token.as_bytes(),
    )?;
    log(
        plan,
        "Ready; waiting for safe-exit authorization and pinned process exit",
    );
    wait_for_exit(plan, session, || parent.exited(), WAIT_EXIT)?;
    // Revalidate after waiting: neither staged files nor the old installation
    // can change unnoticed while a user resolves an unsaved edit.
    if hash_file(&plan.install_root.join(MANIFEST))? != plan.old_manifest_sha256
        || hash_file(&plan.executable)? != plan.parent_sha256
    {
        return Err(invalid("等待退出期间安装文件已改变"));
    }
    validate_staged(plan)?;
    log(
        plan,
        "Original process exited with authorization; replacing program files",
    );
    let transaction = replace_files(plan, &old, &new);
    let changes = match transaction {
        Ok(changes) => changes,
        Err(error) => {
            log(plan, &format!("Replacement failed: {error}"));
            if let Err(restart) = restart_old(plan) {
                return Err(invalid(format!("{error}；旧版未重启：{restart}")));
            }
            return Err(error);
        }
    };
    log(
        plan,
        "Replacement verified; restarting same executable with original data root",
    );
    let restarted = restart(plan);
    let mut child = match restarted {
        Ok(child) => child,
        Err(error) => {
            rollback(plan, &changes)?;
            let _ = restart_old(plan)?;
            return Err(invalid(format!("新版无法启动，已恢复旧版：{error}")));
        }
    };
    if let Err(error) = wait_ready(plan, &mut child) {
        log(plan, &format!("New host health failed: {error}"));
        // Only the process we spawned may be asked to exit, and only through
        // the host's normal shutdown endpoint. Never kill a live data owner.
        if child.try_wait()?.is_none() {
            request_exit(plan, child.id());
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait()?.is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        if child.try_wait()?.is_none() {
            return Err(invalid(format!(
                "新版未就绪且未正常退出，已保留备份，未强制终止。{error}"
            )));
        }
        rollback(plan, &changes)?;
        let _ = restart_old(plan)?;
        return Err(invalid(format!("新版启动检查失败，已恢复旧版：{error}")));
    }
    log(
        plan,
        "Update complete; verified new version, renderer, data root and runtime readiness",
    );
    cleanup_download(plan);
    Ok(())
}

fn cleanup_download(plan: &Plan) {
    // Remove only the known flat download/payload. Keep the old program,
    // journal and logs for recovery. Loaded helper binaries remain until exit.
    let archive = plan.stage.join("package.zip");
    if archive.exists() && reject_links(&archive).is_ok() {
        let _ = fs::remove_file(archive);
    }
    let payload = plan.stage.join("payload");
    let cleanup = (|| -> Result<()> {
        reject_links(&payload)?;
        for entry in fs::read_dir(&payload)? {
            let path = entry?.path();
            reject_links(&path)?;
            if !fs::metadata(&path)?.is_file() {
                return Err(invalid("暂存目录包含未知子目录"));
            }
            fs::remove_file(path)?;
        }
        fs::remove_dir(payload)?;
        Ok(())
    })();
    if let Err(error) = cleanup {
        log(plan, &format!("Non-fatal staging cleanup: {error}"));
    }
}

fn wait_for_exit(
    plan: &Plan,
    session: &str,
    mut exited: impl FnMut() -> Result<bool>,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if signaled(plan, "cancel", session)? {
            return Err(invalid("更新已取消，安装文件未改动"));
        }
        let authorized = signaled(plan, "authorize", session)?;
        if exited()? {
            if !authorized {
                return Err(invalid("原程序退出前未授权更新，安装文件未改动"));
            }
            // Cancellation wins even if it arrived together with process exit.
            if signaled(plan, "cancel", session)? {
                return Err(invalid("更新已取消"));
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(invalid("等待正常退出超时，安装文件未改动"));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[derive(Clone, Debug, Serialize)]
struct Change {
    name: String,
    had_original: bool,
}

fn replacement_names(old: &Manifest, new: &Manifest) -> BTreeSet<String> {
    old.files
        .iter()
        .chain(new.files.iter())
        .map(|f| f.name.clone())
        .chain(METADATA.iter().map(|s| s.to_string()))
        .collect()
}

fn replace_files(plan: &Plan, old: &Manifest, new: &Manifest) -> Result<Vec<Change>> {
    verify_payload(&plan.install_root, old)?;
    let backup = plan.stage.join("rollback");
    fs::create_dir(&backup)?;
    let new_names: BTreeSet<_> = new
        .files
        .iter()
        .map(|f| f.name.clone())
        .chain(METADATA.iter().map(|s| s.to_string()))
        .collect();
    let mut changes = Vec::new();
    let result = (|| {
        for name in replacement_names(old, new) {
            let destination = plan.install_root.join(&name);
            let had_original = destination.exists();
            if had_original {
                reject_links(&destination)?;
                if !destination.is_file() {
                    return Err(invalid("安装目标不是普通文件"));
                }
            }
            let incoming = plan.stage.join("payload").join(&name);
            let replacement = plan.stage.join(format!("incoming-{name}"));
            if new_names.contains(&name) {
                reject_links(&incoming)?;
                fs::copy(&incoming, &replacement)?;
                OpenOptions::new()
                    .write(true)
                    .open(&replacement)?
                    .sync_all()?;
            }
            // The durable journal precedes every mutation. Preserve rollback
            // and journal even on success; they also explain interrupted updates.
            let change = Change {
                name: name.clone(),
                had_original,
            };
            let journal = plan.stage.join("journal.jsonl");
            let mut file = OpenOptions::new().create(true).append(true).open(journal)?;
            serde_json::to_writer(&mut file, &change)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            if had_original {
                fs::rename(&destination, backup.join(&name))?;
            }
            changes.push(change);
            if new_names.contains(&name) {
                fs::rename(replacement, destination)?;
            }
        }
        let actual = read_manifest(&plan.install_root)?;
        validate_manifest(&actual, &plan.new_version, true)?;
        verify_payload(&plan.install_root, &actual)?;
        if hash_file(&plan.install_root.join(MANIFEST))? != plan.new_manifest_sha256 {
            return Err(invalid("安装后的清单不匹配"));
        }
        Ok(())
    })();
    if let Err(error) = result {
        match rollback(plan, &changes) {
            Ok(()) => return Err(invalid(format!("替换失败，旧版文件已恢复：{error}"))),
            Err(rollback) => {
                return Err(invalid(format!(
                    "替换失败：{error}；部分回滚失败：{rollback}"
                )))
            }
        }
    }
    Ok(changes)
}

fn rollback(plan: &Plan, changes: &[Change]) -> Result<()> {
    log(plan, "Rolling back program files; user data is untouched");
    let mut failures = Vec::new();
    for change in changes.iter().rev() {
        let destination = plan.install_root.join(&change.name);
        let result = (|| -> Result<()> {
            if destination.exists() {
                reject_links(&destination)?;
                fs::remove_file(&destination)?;
            }
            if change.had_original {
                let saved = plan.stage.join("rollback").join(&change.name);
                reject_links(&saved)?;
                fs::rename(saved, destination)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            failures.push(format!("{}: {error}", change.name));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(invalid(format!(
            "旧版备份保留在 {}：{}",
            plan.stage.join("rollback").display(),
            failures.join("；")
        )))
    }
}

fn restart(plan: &Plan) -> Result<Child> {
    let mut command = Command::new(&plan.executable);
    command
        .arg("--data-root")
        .arg(&plan.data_root)
        .arg("--api-port")
        .arg(plan.api_port.to_string())
        .env("BETTERCLIP_TAURI_DATA_ROOT", &plan.data_root)
        .env(
            "INTELLITOOLS_CLIPHOST_TAURI_PORT",
            plan.api_port.to_string(),
        )
        .current_dir(&plan.install_root);
    hide(&mut command);
    Ok(command.spawn()?)
}

fn restart_old(plan: &Plan) -> Result<Child> {
    if hash_file(&plan.executable)? != plan.parent_sha256
        || hash_file(&plan.install_root.join(MANIFEST))? != plan.old_manifest_sha256
    {
        return Err(invalid("旧版恢复校验未通过，已保留备份，未启动混合版本"));
    }
    package::validate_installed(&plan.executable, &plan.old_version)?;
    restart(plan)
}

fn health_client() -> Result<reqwest::blocking::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

fn wait_ready(plan: &Plan, child: &mut Child) -> Result<()> {
    let client = health_client()?;
    let base = format!("http://127.0.0.1:{}", plan.api_port);
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Err(invalid("新程序提前退出"));
        }
        let read = |path: &str| -> Option<serde_json::Value> {
            let response = client
                .get(format!("{base}{path}"))
                .send()
                .ok()?
                .error_for_status()
                .ok()?;
            if response.content_length().is_some_and(|n| n > 256 * 1024) {
                return None;
            }
            let mut bytes = Vec::new();
            std::io::Read::take(response, 256 * 1024 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > 256 * 1024 {
                return None;
            }
            serde_json::from_slice(&bytes).ok()
        };
        if let (Some(health), Some(info), Some(paths)) = (
            read("/api/health"),
            read("/api/health/app-info"),
            read("/api/health/data-paths"),
        ) {
            let version = info
                .pointer("/clipHost/fileVersion")
                .and_then(|v| v.as_str())
                .and_then(|v| Version::parse(v).ok());
            let data = paths
                .get("dataRoot")
                .and_then(|v| v.as_str())
                .map(Path::new);
            if health["service"] == "intellitools-cliphost-tauri"
                && health["processId"].as_u64() == Some(child.id() as u64)
                && health["ok"] == true
                && health["windowRuntimeReady"] == true
                && health["isDebugConfiguration"] == false
                && info.pointer("/clipHost/runtime").and_then(|v| v.as_str()) == Some("gpui+rust")
                && version == Some(Version::parse(&plan.new_version)?)
                && data.is_some_and(|p| platform::same_path(p, &plan.data_root).unwrap_or(false))
            {
                // Pin the spawned PID and executable too; an unrelated listener
                // alone is never proof of a successful process replacement.
                platform::Process::open(child.id(), &plan.executable, None)?;
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err(invalid("新版未在45秒内通过版本、数据根和就绪检查"))
}

fn request_exit(plan: &Plan, child_pid: u32) {
    if let Ok(client) = health_client() {
        if platform::Process::open(child_pid, &plan.executable, None).is_err() {
            return;
        }
        let address = format!("http://127.0.0.1:{}", plan.api_port);
        let health = (|| -> Option<serde_json::Value> {
            let response = client
                .get(format!("{address}/api/health"))
                .send()
                .ok()?
                .error_for_status()
                .ok()?;
            if response.content_length().is_some_and(|n| n > 256 * 1024) {
                return None;
            }
            let mut bytes = Vec::new();
            response.take(256 * 1024 + 1).read_to_end(&mut bytes).ok()?;
            if bytes.len() > 256 * 1024 {
                return None;
            }
            serde_json::from_slice(&bytes).ok()
        })();
        if !health.is_some_and(|h| {
            h["service"] == "intellitools-cliphost-tauri"
                && h["processId"].as_u64() == Some(child_pid as u64)
        }) {
            return;
        }
        let _ = client
            .post(format!(
                "http://127.0.0.1:{}/api/clip/chrome",
                plan.api_port
            ))
            .header("Origin", format!("http://127.0.0.1:{}", plan.api_port))
            .json(&serde_json::json!({"kind":"mainHostCommand","command":"exit"}))
            .send();
    }
}

#[cfg(test)]
pub(crate) mod tests;
