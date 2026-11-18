use crate::{
    options::{Action, Options},
    Failure,
};
use serde_json::{json, Value};
use std::{
    fs, io,
    os::windows::{ffi::OsStrExt, fs::MetadataExt, io::AsRawHandle},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, NamedPipeClient},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY},
    System::{
        Pipes::GetNamedPipeServerProcessId,
        RemoteDesktop::ProcessIdToSessionId,
        Threading::{
            CreateProcessW, OpenProcess, QueryFullProcessImageNameW, CREATE_NO_WINDOW,
            PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
        },
    },
    UI::WindowsAndMessaging::{AllowSetForegroundWindow, GetForegroundWindow},
};

// Existing product endpoint and framing. No Quicker process is required.
const PIPE: &str = r"\\.\pipe\IntelliTools_ClipHost_Tauri_PipeRpc_ITCP2026";
const MAX_FRAME: usize = 1024 * 1024;

fn local(message: impl ToString) -> Failure {
    Failure::new("local_error", message, 6)
}
fn transport(error: io::Error) -> Failure {
    Failure::new("transport_error", error, 6)
}

fn plain_file(path: &Path) -> bool {
    // Inspect ancestors before the file itself; never hydrate cloud placeholders.
    if path.components().any(|part| {
        let name = part.as_os_str().to_string_lossy().to_lowercase();
        name.starts_with("onedrive") || name == "dropbox" || name.starts_with("icloud")
    }) {
        return false;
    }
    for parent in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let Ok(metadata) = fs::symlink_metadata(parent) else {
            return false;
        };
        if metadata.file_attributes() & 0x441400 != 0 {
            return false;
        }
    }
    path.is_file()
}

fn executable(path: &Path) -> Result<PathBuf, Failure> {
    let full = std::path::absolute(path).map_err(local)?;
    if !plain_file(&full) {
        return Err(local(format!(
            "Host executable is missing, linked or offline: {}",
            path.display()
        )));
    }
    fs::canonicalize(full).map_err(local)
}

struct HostTarget {
    image: PathBuf,
    data_root: Option<PathBuf>,
}

fn append_argument(line: &mut Vec<u16>, value: &std::ffi::OsStr) {
    if !line.is_empty() {
        line.push(b' ' as u16);
    }
    line.push(b'"' as u16);
    let mut slashes = 0;
    for unit in value.encode_wide() {
        if unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        line.extend(std::iter::repeat_n(
            b'\\' as u16,
            if unit == b'"' as u16 {
                2 * slashes + 1
            } else {
                slashes
            },
        ));
        slashes = 0;
        line.push(unit);
    }
    line.extend(std::iter::repeat_n(b'\\' as u16, 2 * slashes));
    line.push(b'"' as u16);
}

fn launch(host: &HostTarget, script_test: bool) -> Result<(), Failure> {
    let image: Vec<_> = host.image.as_os_str().encode_wide().chain([0]).collect();
    let directory: Vec<_> = host
        .image
        .parent()
        .unwrap()
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect();
    let mut line = Vec::new();
    append_argument(&mut line, host.image.as_os_str());
    append_argument(&mut line, std::ffi::OsStr::new("--background"));
    if let Some(root) = &host.data_root {
        append_argument(&mut line, std::ffi::OsStr::new("--data-root"));
        append_argument(&mut line, root.as_os_str());
    }
    if script_test {
        append_argument(&mut line, std::ffi::OsStr::new("--script-test-instance"));
        append_argument(&mut line, std::ffi::OsStr::new("--api-port"));
        append_argument(&mut line, std::ffi::OsStr::new("0"));
    }
    if line.contains(&0) {
        return Err(local("Host arguments cannot contain NUL."));
    }
    line.push(0);
    unsafe {
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..std::mem::zeroed()
        };
        let mut process: PROCESS_INFORMATION = std::mem::zeroed();
        // bInheritHandles=false is essential: a background host must not keep
        // the calling program's captured stdout/stderr pipes alive after CLI exit.
        if CreateProcessW(
            image.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NO_WINDOW,
            std::ptr::null(),
            directory.as_ptr(),
            &startup,
            &mut process,
        ) == 0
        {
            return Err(transport(io::Error::last_os_error()));
        }
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}

fn hosts(explicit: Option<&Path>) -> Result<Vec<HostTarget>, Failure> {
    if let Some(path) = explicit {
        return executable(path).map(|image| {
            vec![HostTarget {
                image,
                data_root: None,
            }]
        });
    }
    let mut paths = Vec::new();
    let installed = std::env::var_os("LOCALAPPDATA")
        .map(|base| PathBuf::from(base).join("Programs/ClipHostRust"));
    if let Some(root) = &installed {
        let record = root.join("dev-host.json");
        if plain_file(&record) {
            let metadata = fs::metadata(&record).map_err(local)?;
            if metadata.len() > 64 * 1024 {
                return Err(local("Development host registration is too large."));
            }
            let data: Value =
                serde_json::from_slice(&fs::read(&record).map_err(local)?).map_err(local)?;
            if data["schemaVersion"] != 1 || data["profile"] != "debug" || data["apiPort"] != 28965
            {
                return Err(local("Unsupported development host registration."));
            }
            if let Some(path) = data["executable"].as_str().map(PathBuf::from) {
                if path.is_absolute()
                    && path
                        .file_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case("cliphost-gpui.exe"))
                {
                    let data_root = data["dataRoot"].as_str().map(PathBuf::from);
                    if data_root.as_ref().is_some_and(|root| !root.is_absolute()) {
                        return Err(local("Development data root must be absolute."));
                    }
                    paths.push(HostTarget {
                        image: path,
                        data_root,
                    });
                }
            }
        }
    }
    if let Some(parent) = std::env::current_exe().map_err(local)?.parent() {
        paths.push(HostTarget {
            image: parent.join("cliphost-app.exe"),
            data_root: None,
        });
        paths.push(HostTarget {
            image: parent.join("cliphost-gpui.exe"),
            data_root: None,
        });
    }
    if let Some(root) = installed {
        paths.push(HostTarget {
            image: root.join("cliphost-app.exe"),
            data_root: None,
        });
    }
    Ok(paths
        .into_iter()
        .filter_map(|target| {
            executable(&target.image)
                .ok()
                .map(|image| HostTarget { image, ..target })
        })
        .collect())
}

fn peer(stream: &NamedPipeClient, allowed: &[HostTarget]) -> Result<(u32, PathBuf), Failure> {
    unsafe {
        let mut pid = 0;
        if GetNamedPipeServerProcessId(stream.as_raw_handle(), &mut pid) == 0 {
            return Err(transport(io::Error::last_os_error()));
        }
        let (mut ours, mut theirs) = (0, 0);
        if ProcessIdToSessionId(std::process::id(), &mut ours) == 0
            || ProcessIdToSessionId(pid, &mut theirs) == 0
            || ours != theirs
        {
            return Err(local("The pipe server is not in this Windows session."));
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Err(transport(io::Error::last_os_error()));
        }
        let mut image = vec![0u16; 32768];
        let mut size = image.len() as u32;
        let success = QueryFullProcessImageNameW(process, 0, image.as_mut_ptr(), &mut size);
        let error = io::Error::last_os_error();
        CloseHandle(process);
        if success == 0 {
            return Err(transport(error));
        }
        let path = PathBuf::from(String::from_utf16_lossy(&image[..size as usize]));
        let canonical = executable(&path)?;
        if !allowed.iter().any(|expected| {
            expected
                .image
                .as_os_str()
                .eq_ignore_ascii_case(canonical.as_os_str())
        }) {
            return Err(local(format!(
                "Unexpected ClipHost executable: {}. Use --host to explicitly select it.",
                path.display()
            )));
        }
        Ok((pid, path))
    }
}

async fn write_frame(stream: &mut NamedPipeClient, value: &Value) -> Result<(), Failure> {
    let data = serde_json::to_vec(value).map_err(local)?;
    if data.is_empty() || data.len() > MAX_FRAME {
        return Err(local("RPC frame exceeds 1 MiB."));
    }
    // Submit prefix and body together. No flush call or server-side disconnect wait.
    let mut frame = Vec::with_capacity(data.len() + 4);
    frame.extend_from_slice(&(data.len() as u32).to_le_bytes());
    frame.extend(data);
    stream.write_all(&frame).await.map_err(transport)
}

async fn read_frame(stream: &mut NamedPipeClient) -> Result<Value, Failure> {
    let size = stream.read_u32_le().await.map_err(transport)? as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(local("Invalid RPC response frame length."));
    }
    let mut data = vec![0; size];
    stream.read_exact(&mut data).await.map_err(transport)?;
    serde_json::from_slice(&data).map_err(local)
}

fn field<'a>(value: &'a Value, pascal: &str, camel: &str) -> &'a Value {
    value
        .get(pascal)
        .or_else(|| value.get(camel))
        .unwrap_or(&Value::Null)
}

async fn call(
    stream: &mut NamedPipeClient,
    id: i64,
    method: &str,
    payload: Value,
) -> Result<Value, Failure> {
    write_frame(
        stream,
        &json!({"Version":1,"Type":"request","Id":id,"Method":method,"Payload":payload}),
    )
    .await?;
    loop {
        let reply = read_frame(stream).await?;
        if field(&reply, "Version", "version") != 1 {
            return Err(Failure::new(
                "incompatible_protocol",
                "Unsupported host RPC version.",
                6,
            ));
        }
        if field(&reply, "Type", "type") == "request" {
            // The existing endpoint also serves Quicker callbacks. Never identify as a Quicker plugin.
            write_frame(stream, &json!({"Version":1,"Type":"error","Id":field(&reply,"Id","id"),
                "ErrorCode":"method_not_found","ErrorMessage":"CLI does not implement Quicker callbacks."})).await?;
            continue;
        }
        if field(&reply, "Id", "id") != id {
            continue;
        }
        match field(&reply, "Type", "type").as_str() {
            Some("response") => return Ok(field(&reply, "Payload", "payload").clone()),
            Some("error") => {
                return Err(Failure::new(
                    "remote_error",
                    field(&reply, "ErrorMessage", "errorMessage"),
                    5,
                ))
            }
            _ => return Err(local("Invalid RPC response type.")),
        }
    }
}

async fn connect(
    options: &Options,
    allowed: &[HostTarget],
    pipe: &str,
) -> Result<(NamedPipeClient, bool), Failure> {
    let mut launched = false;
    loop {
        match ClientOptions::new().open(pipe) {
            Ok(stream) => return Ok((stream, launched)),
            Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
                if !launched {
                    if options.no_start {
                        return Err(Failure::new("not_running", "ClipHost is not running.", 3));
                    }
                    let host = allowed.first().ok_or_else(|| {
                        Failure::new(
                            "not_installed",
                            "No local host was found; use --host <EXE>.",
                            3,
                        )
                    })?;
                    launch(host, options.script_test)?;
                    launched = true;
                }
                // Existing-instance exits are normal when two callers race to launch.
            }
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {}
            Err(error) => return Err(transport(error)),
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub fn run(options: Options) -> Result<Value, Failure> {
    // Capture the external foreground before creating or showing a host window.
    let foreground = unsafe { GetForegroundWindow() } as isize as i64;
    let expected_root = options
        .expected_root
        .as_deref()
        .map(crate::invocation::checked_root)
        .transpose()
        .map_err(local)?;
    let mut allowed = hosts(options.host.as_deref())?;
    if let Some(root) = &expected_root {
        for host in &mut allowed {
            host.data_root = Some(root.clone());
        }
    }
    let pipe = if options.script_test {
        format!(
            r"\\.\pipe\{}",
            crate::invocation::test_pipe_name(
                expected_root
                    .as_deref()
                    .ok_or_else(|| local("Missing explicit test root."))?
            )
            .map_err(local)?
        )
    } else {
        PIPE.to_owned()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(local)?;
    let mut submitted = false;
    let result = runtime.block_on(async {
        tokio::time::timeout(options.timeout, async {
            let began = Instant::now();
            let (mut stream, started) = connect(&options, &allowed, &pipe).await?;
            let (pid, image) = peer(&stream, &allowed)?;
            let version = call(&mut stream, 1, "cliphost.getFileVersion", Value::Null).await?;
            if !version.is_string() {
                return Err(local("The host returned an invalid version."));
            }
            let connection_ms = began.elapsed().as_secs_f64() * 1000.0;
            let mut response = json!({"ok":true,"state":"ready","message":"ClipHost is ready.",
                "result":{"processId":pid,"executable":image,"version":version,"started":started},
                "connectMs":connection_ms});
            if let Some(root) = &expected_root {
                let info =
                    call(&mut stream, 2, crate::invocation::INFO_METHOD, Value::Null).await?;
                let actual = info["data_root"]
                    .as_str()
                    .ok_or_else(|| local("Invalid explicit invocation information."))?;
                if info["schema_version"] != 1
                    || info["target_context_version"] != 1
                    || !crate::invocation::same_root(root, Path::new(actual)).map_err(local)?
                {
                    return Err(local(
                        "Explicit invocation data root does not match the connected host.",
                    ));
                }
                if options.script_test
                    && (info["test_profile"] != true || info["capture_enabled"] != false)
                {
                    return Err(local(
                        "Script-test host did not confirm the non-capturing QA profile.",
                    ));
                }
                response["result"]["testProfile"] = info["test_profile"].clone();
                response["result"]["captureEnabled"] = info["capture_enabled"].clone();
                response["result"]["dataRoot"] = json!(actual);
                response["result"]["targetContextVersion"] = json!(1);
            }
            let (method, payload) = match &options.action {
                Action::Status | Action::Start => return Ok(response),
                Action::Quit => ("cliphost.requestGracefulShutdown", Value::Null),
                Action::Verb(line) if options.target.is_some() => (
                    crate::invocation::RUN_METHOD,
                    serde_json::to_value(crate::invocation::BoundVerb {
                        schema_version: 1,
                        expected_data_root: expected_root
                            .clone()
                            .ok_or_else(|| local("Missing explicit data root."))?,
                        command: line.clone(),
                        target: options.target.clone().unwrap(),
                    })
                    .map_err(local)?,
                ),
                Action::Verb(line) => (
                    "cliphost.runVerb",
                    json!({"Line":line,"RunContext":{
                    "TargetWindowHandle":foreground,"IsAutomaticRun":false,"TriggerType":0}}),
                ),
            };
            if matches!(options.action, Action::Verb(_)) {
                unsafe {
                    AllowSetForegroundWindow(pid);
                }
            }
            if let Some(target) = &options.target {
                target
                    .validate_at(crate::invocation::now_ms())
                    .map_err(local)?;
            }
            submitted = true;
            let accepted = call(
                &mut stream,
                if expected_root.is_some() { 3 } else { 2 },
                method,
                payload,
            )
            .await?;
            if options.target.is_some() {
                if accepted["accepted"] != true
                    || accepted["target_context_verified"] != true
                    || accepted["scope"] != "existing_verb_accepted"
                {
                    return Err(local("Invalid explicit-target command acceptance."));
                }
                let actual = accepted["data_root"]
                    .as_str()
                    .ok_or_else(|| local("Missing accepted data root."))?;
                if !crate::invocation::same_root(
                    expected_root.as_deref().unwrap(),
                    Path::new(actual),
                )
                .map_err(local)?
                {
                    return Err(local("Accepted data root changed."));
                }
                response["result"]["targetContextVerified"] = json!(true);
            }
            if matches!(options.action, Action::Verb(_))
                && options.target.is_none()
                && accepted != true
            {
                submitted = false; // A definite rejection is not an ambiguous transport failure.
                return Err(Failure::new(
                    "command_rejected",
                    "ClipHost rejected the command.",
                    5,
                ));
            }
            response["state"] = json!("accepted");
            response["message"] = json!("Command accepted by ClipHost.");
            Ok(response)
        })
        .await
    });
    result
        .unwrap_or_else(|_| {
            Err(Failure::new(
                "timeout",
                "The host deadline expired; a submitted command is not retried.",
                4,
            ))
        })
        .map_err(|mut error| {
            error.submitted =
                submitted && (options.target.is_some() || error.code != "remote_error");
            error
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::windows::named_pipe::ServerOptions;

    #[test]
    fn real_pipe_handles_callbacks_partial_frames_and_bounded_stalls() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let name = format!(r"\\.\pipe\eaclip-cli-test-{}", std::process::id());
            let server = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)
                .unwrap();
            let mut client = ClientOptions::new().open(&name).unwrap();
            server.connect().await.unwrap();
            // Verify the real server PID/image rather than trusting JSON identity fields.
            let expected = HostTarget {
                image: executable(&std::env::current_exe().unwrap()).unwrap(),
                data_root: None,
            };
            assert_eq!(peer(&client, &[expected]).unwrap().0, std::process::id());
            assert!(peer(&client, &[]).is_err());
            let (mut reader, mut writer) = tokio::io::split(server);
            let exchange = tokio::spawn(async move {
                let len = reader.read_u32_le().await.unwrap() as usize;
                let mut data = vec![0; len];
                reader.read_exact(&mut data).await.unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&data).unwrap()["Method"],
                    "cliphost.getFileVersion"
                );
                for value in [
                    json!({"Version":1,"Type":"request","Id":99,"Method":"quicker.ping"}),
                    json!({"Version":1,"Type":"response","Id":1,"Payload":"0.32.6"}),
                ] {
                    let data = serde_json::to_vec(&value).unwrap();
                    let frame = [&(data.len() as u32).to_le_bytes()[..], &data].concat();
                    for chunk in frame.chunks(3) {
                        writer.write_all(chunk).await.unwrap();
                    }
                }
                let len = reader.read_u32_le().await.unwrap() as usize;
                let mut data = vec![0; len];
                reader.read_exact(&mut data).await.unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&data).unwrap()["ErrorCode"],
                    "method_not_found"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            });
            assert_eq!(
                call(&mut client, 1, "cliphost.getFileVersion", Value::Null)
                    .await
                    .unwrap(),
                "0.32.6"
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(10), read_frame(&mut client))
                    .await
                    .is_err()
            );
            exchange.await.unwrap();
        });
    }
}
