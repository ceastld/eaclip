//! Explicit, verified updates for the native Windows release. No background installs.
//! The helper is the *currently installed* executable copied outside the replacement
//! set. It cannot replace files until both the safe-exit token and process exit exist.
mod download;
mod install;
mod package;
mod platform;

use package::{validate_installed, Version};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

pub use download::check_official_release;
pub use install::maybe_apply_update_from_args;

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("{0}")]
    Invalid(String),
    #[error("文件操作失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("更新源连接失败：{0}")]
    Network(#[from] reqwest::Error),
    #[error("更新元数据无效：{0}")]
    Json(#[from] serde_json::Error),
    #[error("更新压缩包无效：{0}")]
    Zip(#[from] zip::result::ZipError),
}
pub type Result<T> = std::result::Result<T, UpdateError>;
pub(crate) fn invalid(text: impl Into<String>) -> UpdateError {
    UpdateError::Invalid(text.into())
}

#[derive(Clone, Debug)]
pub struct UpdateConfig {
    pub current_version: String,
    pub executable: PathBuf,
    pub data_root: PathBuf,
    pub api_port: u16,
    /// The caller additionally disables updates in fixtures, test instances and Debug.
    pub allow_install: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdatePhase {
    Idle,
    Checking,
    Available,
    UpToDate,
    Downloading,
    Verifying,
    Ready,
    Installing,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSnapshot {
    pub phase: UpdatePhase,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub message: String,
    pub install_allowed: bool,
    pub install_path: PathBuf,
    pub data_root: PathBuf,
    pub release_url: Option<String>,
    pub release_notes: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OfficialRelease {
    pub version: String,
    pub download_urls: Vec<String>,
    pub sha256: String,
    pub size: Option<u64>,
    pub release_url: String,
    pub release_notes: Option<String>,
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct PreparedUpdate {
    pub version: String,
    pub plan_path: PathBuf,
    pub log_path: PathBuf,
    pub(crate) plan_sha256: String,
    pub(crate) session: Arc<Mutex<Option<String>>>,
}

pub struct UpdateService {
    config: UpdateConfig,
    snapshot: Mutex<UpdateSnapshot>,
    operation: Mutex<()>,
    release: Mutex<Option<OfficialRelease>>,
}

impl UpdateService {
    pub fn new(config: UpdateConfig) -> Result<Self> {
        Version::parse(&config.current_version)?;
        let install_path = config
            .executable
            .parent()
            .ok_or_else(|| invalid("无法识别程序目录"))?
            .to_path_buf();
        let restriction = if cfg!(debug_assertions) || !config.allow_install {
            Some("当前为开发或隔离实例，可检查版本；请在正式安装版中更新。".into())
        } else {
            validate_installed(&config.executable, &config.current_version)
                .err()
                .map(|e| format!("当前目录不支持直接更新：{e}"))
        };
        let last_result = install::previous_result(&install_path);
        let snapshot = UpdateSnapshot {
            phase: UpdatePhase::Idle,
            current_version: config.current_version.clone(),
            latest_version: None,
            downloaded_bytes: 0,
            total_bytes: None,
            message: restriction
                .clone()
                .or(last_result)
                .unwrap_or_else(|| "点击检查更新。".into()),
            install_allowed: restriction.is_none(),
            install_path,
            data_root: config.data_root.clone(),
            release_url: None,
            release_notes: None,
        };
        Ok(Self {
            config,
            snapshot: Mutex::new(snapshot),
            operation: Mutex::new(()),
            release: Mutex::new(None),
        })
    }

    pub fn snapshot(&self) -> UpdateSnapshot {
        lock(&self.snapshot).clone()
    }

    pub fn check(&self) -> Result<UpdateSnapshot> {
        let _busy = self
            .operation
            .try_lock()
            .map_err(|_| invalid("已有更新操作正在进行"))?;
        if self.snapshot().phase == UpdatePhase::Installing {
            return Err(invalid("正在准备退出并安装更新"));
        }
        self.set_phase(UpdatePhase::Checking, "正在检查更新…");
        let result = self.check_inner();
        if let Err(error) = &result {
            self.fail(error);
        }
        result
    }

    fn check_inner(&self) -> Result<UpdateSnapshot> {
        let release = download::check_for_version(&self.config.current_version)?;
        let newer =
            Version::parse(&release.version)? > Version::parse(&self.config.current_version)?;
        let mut state = lock(&self.snapshot);
        state.latest_version = Some(release.version.clone());
        state.release_url = Some(release.release_url.clone());
        state.release_notes = release.release_notes.clone();
        state.phase = if newer {
            UpdatePhase::Available
        } else {
            UpdatePhase::UpToDate
        };
        state.downloaded_bytes = 0;
        state.total_bytes = release.size;
        state.message = if newer {
            format!("发现新版本 {}。", release.version)
        } else {
            "当前已是最新版本。".into()
        };
        *lock(&self.release) = newer.then_some(release);
        Ok(state.clone())
    }

    pub fn download_and_prepare(&self) -> Result<PreparedUpdate> {
        let _busy = self
            .operation
            .try_lock()
            .map_err(|_| invalid("已有更新操作正在进行"))?;
        let result = self.prepare_inner();
        if let Err(error) = &result {
            self.fail(error);
        }
        result
    }

    fn prepare_inner(&self) -> Result<PreparedUpdate> {
        if !self.snapshot().install_allowed {
            return Err(invalid(
                "当前为开发实例或程序目录校验未通过，请使用正式安装版更新",
            ));
        }
        if self.snapshot().phase == UpdatePhase::Installing {
            return Err(invalid("正在安装更新"));
        }
        let release = lock(&self.release)
            .clone()
            .ok_or_else(|| invalid("请先检查更新"))?;
        let installed = validate_installed(&self.config.executable, &self.config.current_version)?;
        if Version::parse(&release.version)? <= Version::parse(&installed.version)? {
            return Err(invalid("拒绝安装相同或较旧版本"));
        }
        let root = self
            .config
            .executable
            .parent()
            .ok_or_else(|| invalid("无法识别程序目录"))?;
        package::reject_links(root)?;
        // Stage lives on the installation volume; a protected installation fails
        // here, while the original host is still alive. No implicit elevation.
        let stages = root.join(".cliphost-updates");
        if stages.exists() {
            package::reject_links(&stages)?;
        } else {
            fs::create_dir(&stages)?;
        }
        let stage = stages.join(uuid::Uuid::new_v4().to_string());
        fs::create_dir(&stage)?;
        self.set_phase(UpdatePhase::Downloading, "正在下载更新…");
        let archive = stage.join("package.zip");
        download::download_archive(&release, &archive, |downloaded, total| {
            let mut state = lock(&self.snapshot);
            state.downloaded_bytes = downloaded;
            state.total_bytes = total;
        })?;
        self.set_phase(UpdatePhase::Verifying, "正在校验更新包…");
        let payload = stage.join("payload");
        let manifest = package::extract_archive(&archive, &payload, &release.version)?;
        let prepared = install::prepare(&self.config, &stage, &manifest)?;
        self.set_phase(UpdatePhase::Ready, "更新已校验，正在准备安全重启…");
        Ok(prepared)
    }

    /// Starts a verified helper and waits for its ready handshake. Does not
    /// authorize installation or request application exit.
    pub fn spawn_install_helper(&self, prepared: &PreparedUpdate) -> Result<()> {
        let _busy = self
            .operation
            .try_lock()
            .map_err(|_| invalid("已有更新操作正在进行"))?;
        let result = install::spawn(prepared);
        match &result {
            Ok(()) => self.set_phase(UpdatePhase::Installing, "准备完成，正在等待保存并退出…"),
            Err(error) => self.fail(error),
        }
        result
    }

    /// Call only after all unsaved-edit and database-flush exit gates pass.
    /// The helper still waits for the pinned original process to exit.
    pub fn authorize_install_helper(&self, prepared: &PreparedUpdate) -> Result<()> {
        install::signal(prepared, "authorize")
    }

    pub fn cancel_install_helper(&self, prepared: &PreparedUpdate) -> Result<()> {
        install::signal(prepared, "cancel")?;
        self.set_phase(UpdatePhase::Available, "已取消更新，当前版本继续运行。");
        Ok(())
    }

    fn set_phase(&self, phase: UpdatePhase, message: &str) {
        let mut state = lock(&self.snapshot);
        state.phase = phase;
        state.message = message.into();
    }
    fn fail(&self, error: &UpdateError) {
        self.set_phase(UpdatePhase::Error, &error.to_string());
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;

/// Synthetic-process verification only; absent from normal application builds.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support {
    use super::*;

    pub fn validate_fixture_root(root: &std::path::Path) -> Result<()> {
        validate_fixture_path(&root.join("fixture-boundary-check"))
    }

    fn validate_fixture_path(path: &std::path::Path) -> Result<()> {
        let allowed = std::fs::canonicalize(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp"),
        )?;
        let parent = path.parent().ok_or_else(|| invalid("模拟路径缺少父目录"))?;
        package::reject_links(parent)?;
        if !std::fs::canonicalize(parent)?.starts_with(allowed) {
            return Err(invalid("更新模拟只能写入当前仓库.tmp"));
        }
        Ok(())
    }

    pub fn download_loopback(
        release: &OfficialRelease,
        destination: &std::path::Path,
        progress: impl FnMut(u64, Option<u64>),
    ) -> Result<()> {
        validate_fixture_path(destination)?;
        download::download_loopback(release, destination, progress)
    }

    pub fn extract_downloaded(
        archive: &std::path::Path,
        destination: &std::path::Path,
        version: &str,
    ) -> Result<()> {
        validate_fixture_path(archive)?;
        validate_fixture_path(destination)?;
        package::extract_archive(archive, destination, version)?;
        Ok(())
    }

    pub fn prepare_staged(
        config: &UpdateConfig,
        stage: &std::path::Path,
    ) -> Result<PreparedUpdate> {
        validate_fixture_path(&stage.join("plan.json"))?;
        validate_fixture_path(&config.executable)?;
        package::reject_links(&config.executable)?;
        validate_fixture_root(&config.data_root)?;
        let manifest = package::read_manifest(&stage.join("payload"))?;
        package::validate_manifest(&manifest, &manifest.version, true)?;
        install::prepare_fixture(config, stage, &manifest)
    }
    pub fn session(prepared: &PreparedUpdate) -> Option<String> {
        lock(&prepared.session).clone()
    }
}
