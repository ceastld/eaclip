//! The existing data-folder settings flow: validate, confirm, persist, restart.
use crate::paths::data_bootstrap::{
    data_root_value, require_existing_database, strip_utf8_bom, LegacyDataSources,
};
use crate::paths::{AppPaths, DATA_ROOT_ENV, DATA_ROOT_SIDECAR_FILE};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

pub(crate) const CHANGE_PROMPT: &str = "数据文件夹已修改。保存后需重启本程序才能使用新位置（不会自动迁移已有数据）。\n\n是否保存并立即重启？";

#[derive(Clone)]
pub(crate) struct DataLocation {
    pub default_root: Option<PathBuf>,
    pub program_directory: Option<PathBuf>,
    pub environment_override: Option<String>,
    /// Injected sources keep unit tests independent of the user's production directories.
    pub legacy_sources: Option<LegacyDataSources>,
    pub require_explicit_root: bool,
}

struct PersistedChoice {
    root: PathBuf,
    use_default: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApplyResult {
    Unchanged,
    Cancelled,
    Saved { restart_requested: bool },
}

impl DataLocation {
    pub fn from_environment() -> Self {
        Self {
            default_root: AppPaths::default_sidecar_dir(),
            program_directory: std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf)),
            environment_override: std::env::var(DATA_ROOT_ENV)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            legacy_sources: cfg!(test).then(LegacyDataSources::default),
            require_explicit_root: cfg!(feature = "native-acceptance"),
        }
    }

    pub fn sidecar_path(&self) -> Result<PathBuf, String> {
        self.default_root
            .as_ref()
            .map(|p| p.join(DATA_ROOT_SIDECAR_FILE))
            .ok_or_else(|| "LOCALAPPDATA is not set".into())
    }

    pub fn override_path(&self) -> Result<Option<PathBuf>, String> {
        Ok(self
            .read_choice()?
            .filter(|choice| !choice.use_default)
            .map(|choice| choice.root))
    }

    fn read_choice(&self) -> Result<Option<PersistedChoice>, String> {
        let path = self.sidecar_path()?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("读取数据位置配置 {} 失败: {error}", path.display())),
        };
        let value: Value = serde_json::from_slice(strip_utf8_bom(&bytes))
            .map_err(|e| format!("数据位置配置无效: {e}"))?;
        let raw = data_root_value(&value).ok_or("数据位置配置缺少有效路径；请修复配置后重试。")?;
        let root = PathBuf::from(raw);
        let use_default = match value.get("useDefault") {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err("数据位置配置的默认目录标记无效。".into()),
        };
        if use_default
            && !self
                .default_root
                .as_ref()
                .is_some_and(|default| same_path(&root, default))
        {
            return Err("数据位置配置的默认目录路径不一致。".into());
        }
        Ok(Some(PersistedChoice { root, use_default }))
    }

    pub fn normalize_root(&self, root: &Path) -> Result<PathBuf, String> {
        let root = normalize_absolute(root)?;
        // Resolve existing ancestors so junctions cannot redirect into GPUI or program files.
        let physical = physical_path(&root)?;
        if !AppPaths::from_data_root(root.clone()).is_isolated_from_csharp()
            || !AppPaths::from_data_root(physical.clone()).is_isolated_from_csharp()
        {
            return Err("不能把 ClipHost 数据目录设为 GPUI BetterClipRs 的数据目录。".into());
        }
        if let Some(program) = &self.program_directory {
            let program = physical_path(&normalize_absolute(program)?)?;
            if same_or_under(&physical, &program) {
                return Err("数据文件夹不能使用程序所在目录或其子目录。".into());
            }
        }
        Ok(root)
    }

    pub fn resolve(&self) -> Result<PathBuf, String> {
        if let Some(raw) = &self.environment_override {
            return self.normalize_root(Path::new(raw));
        }
        if self.require_explicit_root {
            return Err(format!(
                "隔离测试必须显式设置 {DATA_ROOT_ENV}，不会探测真实用户数据。"
            ));
        }
        // An existing but unreadable/invalid pointer must never silently select an empty database.
        if let Some(choice) = self.read_choice()? {
            return self.normalize_root(&choice.root);
        }
        let default = self
            .default_root
            .as_ref()
            .ok_or("LOCALAPPDATA is not set")?;
        let sources = match &self.legacy_sources {
            Some(sources) => sources.clone(),
            None => LegacyDataSources::from_default_root(default)?,
        };
        let root = self.normalize_root(
            &sources
                .select_existing()?
                .unwrap_or_else(|| default.clone()),
        )?;
        let use_default = same_path(&root, default);
        // Persist only the choice, without probing writability or creating the old data directory.
        crate::config_file::save(
            &self.sidecar_path()?,
            &json!({"dataRoot":root,"useDefault":use_default}),
        )?;
        Ok(root)
    }

    /// Recheck the old candidates without changing this process's active root or saved choice.
    pub fn legacy_data_root(&self) -> Result<Option<PathBuf>, String> {
        if self.require_explicit_root {
            return Ok(None);
        }
        let sources = match &self.legacy_sources {
            Some(sources) => sources.clone(),
            None => LegacyDataSources::from_default_root(
                self.default_root
                    .as_ref()
                    .ok_or("LOCALAPPDATA is not set")?,
            )?,
        };
        sources
            .select_existing()?
            .map(|root| {
                let root = self.normalize_root(&root)?;
                require_existing_database(&root)?;
                Ok(root)
            })
            .transpose()
    }

    pub fn same_data_root(&self, a: &Path, b: &Path) -> bool {
        let physical = |path: &Path| self.normalize_root(path).and_then(|p| physical_path(&p));
        match (physical(a), physical(b)) {
            (Ok(a), Ok(b)) => same_path(&a, &b),
            _ => false,
        }
    }

    pub fn write_override(&self, root: Option<&Path>) -> Result<(), String> {
        let path = self.sidecar_path()?;
        if let Some(root) = root {
            let root = self.normalize_root(root)?;
            fs::create_dir_all(&root)
                .map_err(|e| format!("无法创建数据文件夹 {}: {e}", root.display()))?;
            // Catch read-only or inaccessible destinations before saving the next startup path.
            tempfile::NamedTempFile::new_in(&root)
                .map_err(|e| format!("数据文件夹不可写 {}: {e}", root.display()))?;
            crate::config_file::save(&path, &json!({"dataRoot":root}))
        } else {
            let root = self.normalize_root(
                self.default_root
                    .as_ref()
                    .ok_or("LOCALAPPDATA is not set")?,
            )?;
            crate::config_file::save(&path, &json!({"dataRoot":root,"useDefault":true}))
                .map_err(|e| format!("恢复默认数据位置失败: {e}"))
        }
    }

    #[cfg(test)]
    pub fn apply(
        &self,
        values: &Value,
        confirm: impl FnOnce() -> bool,
        restart: impl FnOnce() -> Result<bool, String>,
    ) -> Result<ApplyResult, String> {
        self.apply_for_current(values, None, confirm, restart)
    }

    pub fn apply_for_current(
        &self,
        values: &Value,
        current_root: Option<&Path>,
        confirm: impl FnOnce() -> bool,
        restart: impl FnOnce() -> Result<bool, String>,
    ) -> Result<ApplyResult, String> {
        let use_legacy = match values.get("useLegacyDataRoot") {
            Some(value) => value.as_bool().ok_or("旧版数据目录选项必须是布尔值。")?,
            None => false,
        };
        let value = values
            .get("dataFolder")
            .or_else(|| values.get("overrideDataRoot"))
            .or_else(|| values.get("DataRoot"));
        if !use_legacy && value.is_none() {
            return Ok(ApplyResult::Unchanged);
        }
        if self.environment_override.is_some() {
            return Err("环境变量已覆盖数据路径，无法在界面修改。".into());
        }
        let requested = if use_legacy {
            let root = self
                .legacy_data_root()?
                .ok_or("没有检测到含 userdata.db 的旧版数据目录。")?;
            if current_root.is_some_and(|current| self.same_data_root(&root, current)) {
                return Ok(ApplyResult::Unchanged);
            }
            Some(root)
        } else {
            let raw = value
                .and_then(Value::as_str)
                .ok_or("数据文件夹必须是路径文本。")?
                .trim();
            if raw.is_empty() {
                None
            } else {
                Some(self.normalize_root(Path::new(raw))?)
            }
        };
        let previous = self.override_path()?;
        let same = match (&requested, &previous) {
            (None, None) => true,
            (Some(requested), Some(previous)) => self
                .normalize_root(previous)
                .is_ok_and(|old| same_path(requested, &old)),
            _ => false,
        };
        if same {
            return Ok(ApplyResult::Unchanged);
        }
        if !confirm() {
            return Ok(ApplyResult::Cancelled);
        }
        if use_legacy {
            // The folder may disappear while the confirmation dialog is open. Do not recreate it.
            require_existing_database(requested.as_deref().expect("legacy root was selected"))?;
        }
        self.write_override(requested.as_deref())?;
        let restart_requested = restart().map_err(|e| {
            format!("数据位置已保存，但重启失败，当前程序继续使用原位置。请手动重启。{e}")
        })?;
        Ok(ApplyResult::Saved { restart_requested })
    }
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() || path.as_os_str().to_string_lossy().contains('\0') {
        return Err("数据文件夹必须是有效的绝对路径。".into());
    }
    let mut result = PathBuf::new();
    for part in std::path::absolute(path)
        .map_err(|e| e.to_string())?
        .components()
    {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    return Err("无效的数据文件夹路径。".into());
                }
            }
            _ => result.push(part.as_os_str()),
        }
    }
    Ok(result)
}

fn physical_path(path: &Path) -> Result<PathBuf, String> {
    let mut ancestor = path.to_path_buf();
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(&ancestor) {
            Ok(mut resolved) => {
                for name in suffix.into_iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    ancestor
                        .file_name()
                        .ok_or("无效的数据文件夹路径。")?
                        .to_os_string(),
                );
                if !ancestor.pop() {
                    return Err("无效的数据文件夹路径。".into());
                }
            }
            Err(error) => return Err(format!("无法读取数据文件夹 {}: {error}", path.display())),
        }
    }
}

fn comparison(path: &Path) -> String {
    let path = path.to_string_lossy().replace('/', "\\");
    let path = path
        .strip_prefix(r"\\?\")
        .unwrap_or(&path)
        .trim_end_matches('\\');
    if cfg!(windows) {
        path.to_ascii_lowercase()
    } else {
        path.into()
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    comparison(a) == comparison(b)
}
fn same_or_under(path: &Path, parent: &Path) -> bool {
    let parent = comparison(parent);
    let path = comparison(path);
    path == parent || path.starts_with(&(parent + "\\"))
}

pub(crate) fn confirm_change(owner_hwnd: isize) -> bool {
    #[cfg(test)]
    if let Some(answer) = TEST_CONFIRMATION.with(|v| v.get()) {
        return answer;
    }
    if crate::clipboard::is_headless() || !crate::clipboard::native_dialogs_enabled() {
        return false;
    }
    #[cfg(windows)]
    {
        use windows::{
            core::{w, HSTRING, PCWSTR},
            Win32::{
                Foundation::HWND,
                UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONQUESTION, MB_YESNO},
            },
        };
        let text = HSTRING::from(CHANGE_PROMPT);
        unsafe {
            MessageBoxW(
                HWND(owner_hwnd as *mut _),
                PCWSTR(text.as_ptr()),
                w!("ClipHost"),
                MB_YESNO | MB_ICONQUESTION,
            ) == IDYES
        }
    }
    #[cfg(not(windows))]
    {
        let _ = owner_hwnd;
        false
    }
}

#[cfg(test)]
thread_local! { static TEST_CONFIRMATION: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) }; }

#[cfg(test)]
pub(crate) fn with_confirmation<T>(answer: bool, work: impl FnOnce() -> T) -> T {
    struct Reset(Option<bool>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_CONFIRMATION.with(|v| v.set(self.0));
        }
    }
    let _reset = Reset(TEST_CONFIRMATION.with(|v| v.replace(Some(answer))));
    work()
}
