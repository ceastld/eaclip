//! Data-import entry before Tauri, networking, or host state is initialized.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

struct ImportArgs {
    source: PathBuf,
    destination: PathBuf,
    report: PathBuf,
    dry_run: bool,
}

pub(crate) fn run_if_requested() -> Option<i32> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let parsed = parse_arguments(&arguments)?;
    let result = parsed.and_then(|args| {
        execute(&args, |source, destination, dry_run| {
            cliphost_core::csharp_import::import_csharp_data(source, destination, dry_run)
                .and_then(|report| serde_json::to_value(report).map_err(|error| error.to_string()))
        })
    });
    let response = match result {
        Ok(response) => response,
        Err(error) => json!({"ok": false, "error": error}),
    };
    // Release builds can have no console. The report was opened and written
    // before importing; stdout is only a convenience for terminal callers.
    println!("{response}");
    Some(if response["ok"] == true { 0 } else { 1 })
}

fn parse_arguments(arguments: &[OsString]) -> Option<Result<ImportArgs, String>> {
    if !arguments.iter().any(|argument| {
        argument.to_str().is_some_and(|argument| {
            [
                "--import-csharp-data",
                "--import-destination",
                "--import-report",
            ]
            .iter()
            .any(|flag| argument == *flag || argument.starts_with(&format!("{flag}=")))
        })
    }) {
        return None;
    }
    Some((|| {
        let mut source = None;
        let mut destination = None;
        let mut report = None;
        let mut dry_run = false;
        let mut index = 0;
        while index < arguments.len() {
            let argument = &arguments[index];
            let slot = match argument.to_str() {
                Some("--import-csharp-data") => &mut source,
                Some("--import-destination") => &mut destination,
                Some("--import-report") => &mut report,
                Some("--dry-run") if !dry_run => {
                    dry_run = true;
                    index += 1;
                    continue;
                }
                _ => {
                    return Err(format!(
                        "导入参数无效或重复：{}。",
                        argument.to_string_lossy()
                    ))
                }
            };
            if slot.is_some() {
                return Err(format!("导入参数重复：{}。", argument.to_string_lossy()));
            }
            let value = arguments
                .get(index + 1)
                .filter(|value| !value.is_empty() && !value.to_string_lossy().starts_with("--"))
                .ok_or_else(|| format!("导入参数缺少路径：{}。", argument.to_string_lossy()))?;
            *slot = Some(PathBuf::from(value.as_os_str()));
            index += 2;
        }
        Ok(ImportArgs {
            source: source.ok_or("必须提供 --import-csharp-data SOURCE。")?,
            destination: destination.ok_or("必须提供 --import-destination DEST。")?,
            report: report.ok_or("必须提供 --import-report REPORT。")?,
            dry_run,
        })
    })())
}

fn execute(
    arguments: &ImportArgs,
    import: impl FnOnce(&Path, &Path, bool) -> Result<Value, String>,
) -> Result<Value, String> {
    let source = resolve_path(&arguments.source)?;
    let destination = resolve_path(&arguments.destination)?;
    let report_path = resolve_path(&arguments.report)?;
    if under(&report_path, &source) {
        return Err("导入报告不能写入 C# 来源目录或它的路径别名。".into());
    }
    if under(&report_path, &destination) {
        return Err("导入报告必须放在目标数据目录之外，以保持目标目录为空。".into());
    }
    let mut report_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&report_path)
        .map_err(|error| format!("无法创建新的导入报告 {}：{error}", report_path.display()))?;
    // Verify an actual durable write before the coordinator can publish data.
    // A process interruption leaves a truthful error rather than an empty file.
    write_report(
        &mut report_file,
        &json!({"ok": false, "error": "导入尚未完成。"}),
    )?;
    let response = match import(&source, &destination, arguments.dry_run) {
        Ok(report) => json!({"ok": true, "report": report}),
        Err(error) => json!({"ok": false, "error": error}),
    };
    write_report(&mut report_file, &response)?;
    Ok(response)
}

fn write_report(file: &mut File, response: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(response).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.write_all(&bytes))
        .and_then(|_| file.set_len(bytes.len() as u64))
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("无法写入导入报告：{error}"))
}

/// Canonicalize every existing ancestor before appending missing components.
/// Processing `..` after a junction/symlink resolves aliases of the source too.
/// The resolved path, rather than the original alias, is used for create_new.
fn resolve_path(path: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err("导入路径为空。".into());
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => continue,
            Component::ParentDir => {
                resolved.pop();
                continue;
            }
            Component::Normal(name) => resolved.push(name),
        }
        match fs::canonicalize(&resolved) {
            Ok(canonical) => resolved = canonical,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("无法解析导入路径 {}：{error}", resolved.display())),
        }
    }
    Ok(resolved)
}

fn under(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let path = path.to_string_lossy().replace('/', "\\").to_lowercase();
        let root = root.to_string_lossy().replace('/', "\\").to_lowercase();
        let root = root.trim_end_matches('\\');
        path == root || path.starts_with(&format!("{root}\\"))
    }
    #[cfg(not(windows))]
    {
        path.starts_with(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(|value| OsString::from(*value)).collect()
    }

    #[test]
    fn import_cli_only_claims_import_arguments_and_keeps_all_required_paths() {
        for arguments in [
            &[][..],
            &["--action-id", "action", "--verb", "show"][..],
            &["--dry-run"][..],
        ] {
            assert!(parse_arguments(&strings(arguments)).is_none());
        }
        let arguments = parse_arguments(&strings(&[
            "--import-csharp-data",
            "source folder",
            "--dry-run",
            "--import-destination",
            "destination",
            "--import-report",
            "report.json",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(arguments.source, PathBuf::from("source folder"));
        assert_eq!(arguments.destination, PathBuf::from("destination"));
        assert_eq!(arguments.report, PathBuf::from("report.json"));
        assert!(arguments.dry_run);
    }

    #[test]
    fn import_cli_rejects_missing_duplicate_and_mixed_application_arguments() {
        for arguments in [
            vec!["--import-csharp-data", "source"],
            vec!["--import-csharp-data", "--import-report", "report"],
            vec![
                "--import-csharp-data",
                "source",
                "--import-csharp-data",
                "other",
            ],
            vec!["--import-destination", "destination", "--verb", "show"],
            vec!["--import-csharp-data=source", "--import-report=report"],
        ] {
            assert!(parse_arguments(&strings(&arguments)).unwrap().is_err());
        }
    }

    #[test]
    fn import_cli_report_rejects_source_aliases_and_never_overwrites() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("nested")).unwrap();
        let mut arguments = ImportArgs {
            source: source.clone(),
            destination: temporary.path().join("destination"),
            report: source.join("nested").join("..").join("report.json"),
            dry_run: true,
        };
        let called = AtomicBool::new(false);
        assert!(execute(&arguments, |_, _, _| {
            called.store(true, Ordering::Relaxed);
            Ok(json!({}))
        })
        .is_err());
        assert!(!called.load(Ordering::Relaxed));
        assert!(!source.join("report.json").exists());
        assert!(under(
            &resolve_path(&source.join("new").join("..").join("report.json")).unwrap(),
            &resolve_path(&source).unwrap()
        ));
        #[cfg(windows)]
        assert!(under(
            Path::new(r"C:\Source\child.json"),
            Path::new(r"c:\source")
        ));
        #[cfg(unix)]
        {
            let alias = temporary.path().join("alias");
            std::os::unix::fs::symlink(&source, &alias).unwrap();
            arguments.report = alias.join("report.json");
            assert!(execute(&arguments, |_, _, _| panic!("alias must be rejected")).is_err());
        }
        arguments.report = temporary.path().join("existing.json");
        fs::write(&arguments.report, b"keep original").unwrap();
        assert!(execute(&arguments, |_, _, _| panic!(
            "existing report must be rejected"
        ))
        .is_err());
        assert_eq!(fs::read(&arguments.report).unwrap(), b"keep original");
    }

    #[test]
    fn import_cli_writes_report_before_import_and_persists_success_and_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        fs::create_dir(&source).unwrap();
        let mut arguments = ImportArgs {
            source: source.clone(),
            destination: temporary.path().join("destination"),
            report: temporary.path().join("failure.json"),
            dry_run: true,
        };
        let response = execute(&arguments, |actual_source, destination, dry_run| {
            assert_eq!(actual_source, resolve_path(&source).unwrap());
            assert_eq!(destination, resolve_path(&arguments.destination).unwrap());
            assert!(dry_run);
            let initial: Value =
                serde_json::from_slice(&fs::read(&arguments.report).unwrap()).unwrap();
            assert_eq!(initial["ok"], false);
            Err("历史条目 row-id：未知类型。".into())
        })
        .unwrap();
        assert_eq!(
            response,
            json!({"ok":false,"error":"历史条目 row-id：未知类型。"})
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&arguments.report).unwrap()).unwrap(),
            response
        );
        arguments.report = temporary.path().join("success.json");
        arguments.dry_run = false;
        let response = execute(&arguments, |_, _, dry_run| {
            assert!(!dry_run);
            Ok(json!({"clips":3,"favorites":2,"images":1,"dryRun":false}))
        })
        .unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(response["report"]["clips"], 3);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&arguments.report).unwrap()).unwrap(),
            response
        );
    }
}
