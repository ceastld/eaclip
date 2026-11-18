//! A console client, never a second clipboard engine or GUI application.
#[path = "../../../shared/invocation.rs"]
mod invocation;
mod options;
#[cfg(windows)]
mod windows;

use serde_json::{json, Value};
use std::time::Instant;

#[derive(Debug)]
pub struct Failure {
    code: &'static str,
    message: String,
    exit_code: u8,
    /// Set before the command frame is written: an interrupted write is ambiguous.
    submitted: bool,
}

impl Failure {
    pub fn new(code: &'static str, message: impl ToString, exit_code: u8) -> Self {
        Self {
            code,
            message: message.to_string(),
            exit_code,
            submitted: false,
        }
    }
}

fn main() -> std::process::ExitCode {
    let began = Instant::now();
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let machine = arguments.iter().any(|value| value == "--json");
    let result = options::Options::parse(arguments).and_then(|options| match options {
        options::Parsed::Help => {
            println!("{}", options::HELP);
            Ok(None)
        }
        options::Parsed::Version => {
            println!("eaclip-cli {}", env!("CARGO_PKG_VERSION"));
            Ok(None)
        }
        options::Parsed::Run(options) => run(options).map(Some),
    });
    match result {
        Ok(Some(mut response)) => {
            response["totalMs"] = json!(began.elapsed().as_secs_f64() * 1000.0);
            if machine {
                println!("{response}");
            } else {
                println!("{}", response["message"].as_str().unwrap_or("OK"));
            }
            std::process::ExitCode::SUCCESS
        }
        Ok(None) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            if machine {
                println!(
                    "{}",
                    json!({"ok":false,"state":if error.submitted {"unknown"} else {"failed"},
                    "submitted":error.submitted,"error":{"code":error.code,"message":error.message},
                    "totalMs":began.elapsed().as_secs_f64()*1000.0})
                );
            } else {
                eprintln!("{}: {}", error.code, error.message);
            }
            std::process::ExitCode::from(error.exit_code)
        }
    }
}

#[cfg(windows)]
fn run(options: options::Options) -> Result<Value, Failure> {
    windows::run(options)
}

#[cfg(not(windows))]
fn run(_: options::Options) -> Result<Value, Failure> {
    Err(Failure::new(
        "unsupported_platform",
        "ClipHost CLI currently supports Windows.",
        6,
    ))
}
