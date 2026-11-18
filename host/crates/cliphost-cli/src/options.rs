use crate::Failure;
use std::{ffi::OsString, path::PathBuf, time::Duration};

pub const HELP: &str = "eaclip-cli — independent ClipHost command-line client

Usage: eaclip-cli <command> [options]
  status | ping              Query the running host; never start it
  start                      Ensure the host is running in the background
  show | hide | toggle       Control the clipboard window
  search                     Open CeaBox aggregate search
  search-toggle              Show or hide CeaBox (for external hotkeys)
  favorites                  Show favorites
  settings                   Open settings
  menu                       Show the super menu
  quit                       Request graceful shutdown; never starts a host
  verb <LINE>                Execute one existing ClipHost verb, e.g. \"clip show --focus-search\"

Options:
  --json                     One structured result on stdout
  --host <EXE>               Explicit host executable (also used to verify the connected PID)
  --no-start                 Fail if the host is not running
  --expected-data-root PATH  Bind new invocation to this exact data root
  --test-data-root PATH      Use explicit isolated script-test host and private pipe
  --target-context JSON     Required five-field original target for explicit-root verbs
  --timeout-ms <N>           Total connection/start/request deadline (default 15000)
  --help | -h                Show this help
  --version | -V             Show CLI version without contacting a host

Success for window commands means the existing host accepted the command,
not that the window has rendered. A timeout after submission reports unknown;
the CLI never automatically resends a submitted command.";

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Status,
    Start,
    Quit,
    Verb(String),
}

pub struct Options {
    pub action: Action,
    pub host: Option<PathBuf>,
    pub no_start: bool,
    pub timeout: Duration,
    pub expected_root: Option<PathBuf>,
    pub script_test: bool,
    pub target: Option<crate::invocation::TargetSnapshot>,
}

pub enum Parsed {
    Help,
    Version,
    Run(Options),
}

fn invalid(message: impl ToString) -> Failure {
    Failure::new("invalid_arguments", message, 2)
}

impl Options {
    pub fn parse(arguments: Vec<OsString>) -> Result<Parsed, Failure> {
        let mut host = None;
        let mut no_start = false;
        let mut timeout = Duration::from_secs(15);
        let mut expected_root = None;
        let mut script_test = false;
        let mut target = None;
        let mut words = Vec::new();
        let mut args = arguments.into_iter();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--help" | "-h") => return Ok(Parsed::Help),
                Some("--version" | "-V") => return Ok(Parsed::Version),
                Some("--json") => {}
                Some("--no-start") => no_start = true,
                Some("--host") => {
                    if host.is_some() {
                        return Err(invalid("--host may only be specified once."));
                    }
                    host =
                        Some(PathBuf::from(args.next().ok_or_else(|| {
                            invalid("--host requires an executable path.")
                        })?));
                }
                Some("--expected-data-root" | "--test-data-root") => {
                    if expected_root.is_some() {
                        return Err(invalid("Only one explicit data root is allowed."));
                    }
                    script_test = arg == "--test-data-root";
                    let root = PathBuf::from(
                        args.next()
                            .ok_or_else(|| invalid("Explicit data root is missing."))?,
                    );
                    crate::invocation::validate_root_spelling(&root).map_err(invalid)?;
                    expected_root = Some(root);
                }
                Some("--target-context") => {
                    if target.is_some() {
                        return Err(invalid("--target-context may only appear once."));
                    }
                    let raw = args
                        .next()
                        .ok_or_else(|| invalid("Target context is missing."))?;
                    let raw = raw
                        .to_str()
                        .filter(|value| value.len() <= 4096)
                        .ok_or_else(|| invalid("Invalid target context text."))?;
                    let value: crate::invocation::TargetSnapshot =
                        serde_json::from_str(raw).map_err(invalid)?;
                    value
                        .validate_at(crate::invocation::now_ms())
                        .map_err(invalid)?;
                    target = Some(value);
                }
                Some("--timeout-ms") => {
                    let raw = args
                        .next()
                        .ok_or_else(|| invalid("--timeout-ms requires a value."))?;
                    let value = raw
                        .to_str()
                        .and_then(|v| v.parse::<u64>().ok())
                        .filter(|v| (1..=120_000).contains(v))
                        .ok_or_else(|| invalid("--timeout-ms must be between 1 and 120000."))?;
                    timeout = Duration::from_millis(value);
                }
                Some(value) if !value.starts_with('-') => words.push(value.to_string()),
                _ => {
                    return Err(invalid(format!(
                        "Unknown argument: {}",
                        arg.to_string_lossy()
                    )))
                }
            }
        }
        let command = words
            .first()
            .map(String::as_str)
            .ok_or_else(|| invalid("A command is required. Use --help."))?;
        if command != "verb" && words.len() != 1 {
            return Err(invalid(
                "This command takes no positional arguments; use verb for existing host options.",
            ));
        }
        let action = match command {
            "status" | "ping" => {
                no_start = true;
                Action::Status
            }
            "start" => Action::Start,
            "quit" => {
                no_start = true;
                Action::Quit
            }
            "show" => Action::Verb("clip show".into()),
            "hide" => Action::Verb("clip hide".into()),
            "toggle" => Action::Verb("clip toggle".into()),
            "search" => Action::Verb("search show".into()),
            "search-toggle" => Action::Verb("search show_or_hide".into()),
            "favorites" => Action::Verb("favor show".into()),
            "settings" => Action::Verb("setting global".into()),
            "menu" => Action::Verb("menu".into()),
            "verb"
                if words.len() == 2
                    && !words[1].trim().is_empty()
                    && words[1].len() <= 64 * 1024
                    && !words[1].contains('\0') =>
            {
                Action::Verb(words.remove(1))
            }
            "verb" => {
                return Err(invalid(
                    "verb requires one nonempty quoted command line (at most 64 KiB).",
                ))
            }
            _ => return Err(invalid(format!("Unknown command: {command}"))),
        };
        if expected_root.is_some() {
            if host.as_ref().is_none_or(|path| !path.is_absolute()) {
                return Err(invalid(
                    "Explicit-root invocation requires an absolute --host.",
                ));
            }
            if matches!(&action, Action::Verb(_)) && target.is_none() {
                return Err(invalid(
                    "Explicit-root verbs require --target-context; no foreground fallback.",
                ));
            }
        } else if target.is_some() {
            return Err(invalid(
                "--target-context requires --expected-data-root or --test-data-root.",
            ));
        }
        if target.is_some() && !matches!(&action, Action::Verb(_)) {
            return Err(invalid(
                "Target context is only valid for verb/window commands.",
            ));
        }
        Ok(Parsed::Run(Self {
            action,
            host,
            no_start,
            timeout,
            expected_root,
            script_test,
            target,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(values: &[&str]) -> Result<Options, Failure> {
        match Options::parse(values.iter().map(OsString::from).collect())? {
            Parsed::Run(options) => Ok(options),
            _ => panic!("expected command"),
        }
    }
    #[test]
    fn maps_only_existing_verbs_and_preserves_raw_line() {
        assert_eq!(
            parse(&["show"]).unwrap().action,
            Action::Verb("clip show".into())
        );
        let line = "collect text --data \"A & $() ; 中文\"";
        assert_eq!(
            parse(&["--json", "verb", line]).unwrap().action,
            Action::Verb(line.into())
        );
    }
    #[test]
    fn status_and_quit_never_launch_and_arguments_fail_before_effects() {
        assert!(parse(&["status"]).unwrap().no_start);
        assert!(parse(&["quit"]).unwrap().no_start);
        for args in [
            vec![],
            vec!["verb"],
            vec!["verb", ""],
            vec!["show", "extra"],
            vec!["search-toggle", "extra"],
            vec!["show", "--unknown"],
            vec!["status", "--timeout-ms", "0"],
            vec!["show", "--host"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn search_hotkey_targets_ceabox_without_changing_clipboard_toggle() {
        assert_eq!(
            parse(&["search-toggle", "--no-start", "--json"])
                .unwrap()
                .action,
            Action::Verb("search show_or_hide".into())
        );
        assert!(parse(&["search-toggle", "--no-start"]).unwrap().no_start);
        assert_eq!(
            parse(&["search"]).unwrap().action,
            Action::Verb("search show".into())
        );
        assert_eq!(
            parse(&["toggle"]).unwrap().action,
            Action::Verb("clip toggle".into())
        );
    }
}

#[cfg(test)]
mod explicit_context_tests {
    use super::*;
    fn arguments(test: bool) -> Vec<OsString> {
        let root = std::env::temp_dir().join("cliphost-explicit-options");
        let target = crate::invocation::TargetSnapshot {
            window_handle: "9007199254740993".into(),
            process_id: 2,
            thread_id: 3,
            process_created_file_time: "18446744073709550000".into(),
            expires_at_unix_ms: crate::invocation::now_ms() + 30_000,
        };
        vec![
            "verb".into(),
            "clip toggle".into(),
            "--host".into(),
            std::env::current_exe().unwrap().into_os_string(),
            if test {
                "--test-data-root".into()
            } else {
                "--expected-data-root".into()
            },
            root.into_os_string(),
            "--target-context".into(),
            serde_json::to_string(&target).unwrap().into(),
        ]
    }
    #[test]
    fn explicit_options_preserve_target_and_never_supply_root_or_host_defaults() {
        for test in [false, true] {
            let Parsed::Run(options) = Options::parse(arguments(test)).unwrap() else {
                panic!("run")
            };
            assert_eq!(options.script_test, test);
            assert_eq!(options.target.unwrap().window_handle, "9007199254740993");
            let mut no_host = arguments(test);
            drop(no_host.drain(2..4));
            assert!(Options::parse(no_host).is_err());
            let mut no_root = arguments(test);
            drop(no_root.drain(4..6));
            assert!(Options::parse(no_root).is_err());
            let mut no_target = arguments(test);
            drop(no_target.drain(6..8));
            assert!(Options::parse(no_target).is_err());
        }
        let Parsed::Run(old) = Options::parse(vec!["toggle".into()]).unwrap() else {
            panic!("old")
        };
        assert!(old.target.is_none() && old.expected_root.is_none() && !old.script_test);
    }
}
