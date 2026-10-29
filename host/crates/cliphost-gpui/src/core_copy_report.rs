//! A report owned by one explicit CORE copy acceptance run. The writable handle
//! is acquired once; later snapshots never reopen a caller-supplied path.

use serde_json::Value;
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

const REPORT_NAME: &str = "core-gui-report.json";
static REPORT: Mutex<Option<OwnedReport>> = Mutex::new(None);

pub fn initialize(data_root: &Path, report_path: &Path) -> Result<(), String> {
    let mut report = REPORT.lock().map_err(|_| "CORE report lock is poisoned")?;
    if report.is_some() {
        return Err("CORE report has already been initialized".into());
    }
    *report = Some(OwnedReport::open(data_root, report_path)?);
    Ok(())
}

/// `false` leaves the older QA report mechanism available outside CORE mode.
pub fn write_if_owned(path: &Path, value: &Value) -> Result<bool, String> {
    let mut report = REPORT.lock().map_err(|_| "CORE report lock is poisoned")?;
    let Some(report) = report.as_mut() else {
        return Ok(false);
    };
    report.write(path, value)?;
    Ok(true)
}

struct OwnedReport {
    original_path: PathBuf,
    file: File,
    // No-delete sharing also pins the checked directory route on Windows.
    // These handles live as long as the writable report handle.
    _directories: Vec<File>,
}

impl OwnedReport {
    fn open(data_root: &Path, report_path: &Path) -> Result<Self, String> {
        let data = local_absolute(data_root)?;
        let requested = local_absolute(report_path)?;
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .ok_or("CORE report cannot locate its compiled repository")?;
        let scratch = local_absolute(&repository.join(".tmp"))?;
        let run = data.parent().ok_or("CORE data root has no run parent")?;
        if !strictly_beneath(run, &scratch) {
            return Err(
                "CORE report data must be in a run below the compiled repository .tmp".into(),
            );
        }
        let evidence = run.join("evidence");
        let expected = evidence.join(REPORT_NAME);
        if !same_path(&requested, &expected)
            || report_path.file_name() != Some(std::ffi::OsStr::new(REPORT_NAME))
        {
            return Err("CORE report must be the current run/evidence/core-gui-report.json".into());
        }

        // Check before canonicalization, which would otherwise hide a junction.
        // Evidence must already exist: this writer never creates arbitrary dirs.
        let mut directories = inspect_directory_route(&scratch)?;
        directories.extend(inspect_directory_route(&data)?);
        directories.extend(inspect_directory_route(&evidence)?);
        let actual_scratch = local_absolute(&fs::canonicalize(&scratch).map_err(io_error)?)?;
        let actual_data = local_absolute(&fs::canonicalize(&data).map_err(io_error)?)?;
        let actual_run = actual_data
            .parent()
            .ok_or("CORE data root has no run parent")?;
        let actual_evidence = local_absolute(&fs::canonicalize(&evidence).map_err(io_error)?)?;
        if !strictly_beneath(actual_run, &actual_scratch)
            || !same_path(&actual_evidence, &actual_run.join("evidence"))
        {
            return Err("CORE report resolved outside the current isolated run".into());
        }

        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_OPEN_REPARSE_POINT; FILE_SHARE_READ deliberately omits
            // WRITE and DELETE, preventing another writer or path replacement.
            options.custom_flags(0x0020_0000).share_mode(1);
        }
        let file = options.open(&requested).map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        safe_metadata(&metadata)?;
        if !metadata.is_file() {
            return Err("CORE report must be a regular file".into());
        }
        validate_opened_path(&file, &actual_evidence.join(REPORT_NAME))?;
        file.sync_all().map_err(io_error)?;
        Ok(Self {
            original_path: report_path.to_path_buf(),
            file,
            _directories: directories,
        })
    }

    fn write(&mut self, path: &Path, value: &Value) -> Result<(), String> {
        // Match the originally authorized spelling, not an arbitrary equivalent
        // path that a later caller supplies. Only the retained handle is used.
        if path.as_os_str() != self.original_path.as_os_str() {
            return Err("CORE report write path differs from its initialized path".into());
        }
        let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
        self.file.seek(SeekFrom::Start(0)).map_err(io_error)?;
        self.file.set_len(0).map_err(io_error)?;
        self.file.write_all(&bytes).map_err(io_error)?;
        self.file.write_all(b"\n").map_err(io_error)?;
        self.file.sync_all().map_err(io_error)
    }
}

fn local_absolute(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("CORE report paths must be absolute local paths".into());
    }
    // Verbatim Windows paths do not split '/' like ordinary disk paths do.
    // Check the original spelling before rebuilding a PathBuf: push can turn
    // such a Normal component into a traversing, subsequently normalized path.
    let raw = path.to_str().ok_or("CORE report path has invalid text")?;
    if raw
        .split(|character| character == '/' || character == '\\')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err("CORE report paths may not contain dot or parent traversal segments".into());
    }
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir | Component::CurDir => {
                return Err("CORE report paths may not contain parent traversal".into());
            }
            #[cfg(windows)]
            Component::Prefix(prefix) => {
                use std::path::Prefix;
                match prefix.kind() {
                    Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                        result.push(format!("{}:", drive as char));
                    }
                    _ => return Err("CORE report paths must use a local disk".into()),
                }
            }
            Component::Normal(name) => {
                let name_text = name.to_str().ok_or("CORE report path has invalid text")?;
                let lower = name_text.to_ascii_lowercase();
                if lower.starts_with("onedrive")
                    || lower == "dropbox"
                    || lower.starts_with("icloud")
                {
                    return Err("CORE report refuses cloud directory paths".into());
                }
                #[cfg(windows)]
                if name_text.contains(':') || name_text.ends_with('.') || name_text.ends_with(' ') {
                    return Err(
                        "CORE report path contains an ambiguous name or alternate stream".into(),
                    );
                }
                result.push(name);
            }
            _ => result.push(part.as_os_str()),
        }
    }
    Ok(result)
}

fn same_component(a: Component<'_>, b: Component<'_>) -> bool {
    #[cfg(windows)]
    {
        a.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    let mut left = a.components();
    let mut right = b.components();
    loop {
        match (left.next(), right.next()) {
            (Some(a), Some(b)) if same_component(a, b) => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn strictly_beneath(path: &Path, root: &Path) -> bool {
    let mut parts = path.components();
    for expected in root.components() {
        if !parts
            .next()
            .is_some_and(|actual| same_component(actual, expected))
        {
            return false;
        }
    }
    parts.next().is_some()
}

fn safe_metadata(metadata: &Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() {
        return Err("CORE report refuses symlinks and reparse points".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // REPARSE_POINT, OFFLINE, RECALL_ON_OPEN, RECALL_ON_DATA_ACCESS.
        if metadata.file_attributes() & (0x400 | 0x1000 | 0x40000 | 0x400000) != 0 {
            return Err("CORE report refuses reparse points and cloud placeholders".into());
        }
    }
    Ok(())
}

fn inspect_directory_route(path: &Path) -> Result<Vec<File>, String> {
    let mut current = PathBuf::new();
    let mut pins = Vec::new();
    for part in path.components() {
        current.push(part.as_os_str());
        if matches!(part, Component::Prefix(_)) {
            continue;
        }
        let metadata = fs::symlink_metadata(&current).map_err(io_error)?;
        safe_metadata(&metadata)?;
        if !metadata.is_dir() {
            return Err("CORE report directory route is not an existing directory".into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let pin = OpenOptions::new()
                .read(true)
                .custom_flags(0x0200_0000 | 0x0020_0000)
                .share_mode(1 | 2)
                .open(&current)
                .map_err(io_error)?;
            let metadata = pin.metadata().map_err(io_error)?;
            safe_metadata(&metadata)?;
            if !metadata.is_dir() {
                return Err("CORE report directory route changed while opening".into());
            }
            validate_opened_path(&pin, &current)?;
            pins.push(pin);
        }
    }
    Ok(pins)
}

fn validate_opened_path(file: &File, expected: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetFinalPathNameByHandleW(
                handle: *mut std::ffi::c_void,
                path: *mut u16,
                size: u32,
                flags: u32,
            ) -> u32;
        }
        let mut buffer = vec![0u16; 512];
        loop {
            let length = unsafe {
                GetFinalPathNameByHandleW(
                    file.as_raw_handle(),
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    0,
                )
            };
            if length == 0 {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            if length as usize >= buffer.len() {
                buffer.resize(length as usize + 1, 0);
                continue;
            }
            let actual = PathBuf::from(
                String::from_utf16(&buffer[..length as usize])
                    .map_err(|_| "CORE report opened path has invalid text")?,
            );
            if !same_path(&local_absolute(&actual)?, &local_absolute(expected)?) {
                return Err("CORE report path changed while opening".into());
            }
            break;
        }
    }
    #[cfg(not(windows))]
    let _ = (file, expected);
    Ok(())
}

fn io_error(error: std::io::Error) -> String {
    format!("CORE report I/O failed ({:?})", error.kind())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap();
        let parent = repository.join(".tmp").canonicalize().unwrap();
        let run = tempfile::Builder::new()
            .prefix("core-copy-report-")
            .tempdir_in(parent)
            .unwrap();
        let data = run.path().join("data");
        let evidence = run.path().join("evidence");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&evidence).unwrap();
        (run, data, evidence.join(REPORT_NAME))
    }

    #[test]
    fn core_report_retained_handle_writes_readable_json_and_truncates() {
        let (_run, data, path) = fixture();
        let mut report = OwnedReport::open(&data, &path).unwrap();
        report
            .write(&path, &json!({"longer": "first report", "ready": false}))
            .unwrap();
        report.write(&path, &json!({"ready": true})).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
            json!({"ready": true})
        );
        #[cfg(windows)]
        {
            assert!(OpenOptions::new().write(true).open(&path).is_err());
            assert!(fs::rename(&path, path.with_extension("moved")).is_err());
            assert!(fs::remove_file(&path).is_err());
        }
    }

    #[test]
    fn core_report_existing_target_is_never_overwritten() {
        let (_run, data, path) = fixture();
        fs::write(&path, b"existing-report-sentinel").unwrap();
        assert!(OwnedReport::open(&data, &path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"existing-report-sentinel");
    }

    #[test]
    fn core_report_rejects_other_run_database_config_and_changed_write_paths() {
        let (_run, data, path) = fixture();
        let (_other_run, _, other_report) = fixture();
        let config_dir = data.join("Configs");
        fs::create_dir(&config_dir).unwrap();
        let database = data.join("userdata.db");
        let config = config_dir.join("GlobalConfig.json");
        for forbidden in [&database, &config, &other_report] {
            fs::write(forbidden, b"do-not-change").unwrap();
            assert!(OwnedReport::open(&data, forbidden).is_err());
            assert_eq!(fs::read(forbidden).unwrap(), b"do-not-change");
        }
        let mut report = OwnedReport::open(&data, &path).unwrap();
        report.write(&path, &json!({"original": true})).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(report.write(&database, &json!({"bad": true})).is_err());
        assert_eq!(fs::read(&database).unwrap(), b"do-not-change");
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn core_report_refuses_traversal_relative_cloud_and_non_scratch_data() {
        let (run, data, path) = fixture();
        assert!(OwnedReport::open(&data, Path::new("evidence/core-gui-report.json")).is_err());
        assert!(OwnedReport::open(
            &data,
            // Preserve the spelling supplied to the boundary. A Windows
            // verbatim PathBuf::join can consume '..' before open sees it.
            &PathBuf::from(format!("{}/data/../evidence/{REPORT_NAME}", run.path().display()))
        )
        .is_err());
        let outside = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(OwnedReport::open(outside, &path).is_err());
        let cloud_run = run.path().join("OneDrive-fixture");
        fs::create_dir_all(cloud_run.join("data")).unwrap();
        fs::create_dir(cloud_run.join("evidence")).unwrap();
        let cloud_report = cloud_run.join("evidence").join(REPORT_NAME);
        fs::write(&cloud_report, b"cloud-sentinel").unwrap();
        assert!(OwnedReport::open(&cloud_run.join("data"), &cloud_report).is_err());
        assert_eq!(fs::read(cloud_report).unwrap(), b"cloud-sentinel");
        assert!(!path.exists());
    }

    #[test]
    fn core_report_refuses_raw_dot_segments_with_both_separators() {
        let (run, data, path) = fixture();
        for suffix in [
            "data/../evidence/core-gui-report.json",
            r"data\..\evidence\core-gui-report.json",
            r"data/..\evidence/core-gui-report.json",
            "evidence/./core-gui-report.json",
            r"evidence\.\core-gui-report.json",
        ] {
            // Construct raw text rather than join: Windows verbatim PathBuf
            // joins may normalize backslash dot segments before validation.
            let attempted = PathBuf::from(format!("{}/{}", run.path().display(), suffix));
            assert!(
                OwnedReport::open(&data, &attempted).is_err(),
                "accepted {suffix}"
            );
        }
        let traversing_data = PathBuf::from(format!("{}/evidence/../data", run.path().display()));
        assert!(OwnedReport::open(&traversing_data, &path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn core_report_refuses_reparse_evidence_without_touching_its_target() {
        let (run, data, path) = fixture();
        let evidence = path.parent().unwrap();
        fs::remove_dir(evidence).unwrap();
        let target = run.path().join("actual-evidence");
        fs::create_dir(&target).unwrap();
        let sentinel = target.join(REPORT_NAME);
        fs::write(&sentinel, b"reparse-target-sentinel").unwrap();
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(local_absolute(evidence).unwrap())
                .arg(local_absolute(&target).unwrap())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(
                status.success(),
                "synthetic directory junction could not be created"
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, evidence).unwrap();
        assert!(OwnedReport::open(&data, &path).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"reparse-target-sentinel");
        #[cfg(windows)]
        fs::remove_dir(evidence).unwrap();
        #[cfg(unix)]
        fs::remove_file(evidence).unwrap();
    }
}
