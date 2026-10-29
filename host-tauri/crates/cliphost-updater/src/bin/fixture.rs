//! Explicit test-support binary. Never included in normal builds or packages.
use cliphost_updater::{UpdateConfig, UpdateService};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--apply-update") && args.len() == 6 {
        let plan = PathBuf::from(&args[1]);
        let stage = plan.parent().unwrap();
        cliphost_updater::test_support::validate_fixture_root(stage).unwrap();
        let session = uuid::Uuid::parse_str(&args[5]).unwrap();
        fs::write(
            stage.join(format!("helper-process-{session}.json")),
            serde_json::to_vec(
                &json!({"pid":std::process::id(),"executable":std::env::current_exe().unwrap()}),
            )
            .unwrap(),
        )
        .unwrap();
    }
    if let Some(result) = cliphost_updater::maybe_apply_update_from_args() {
        if let Err(error) = result {
            eprintln!("{error}");
            std::process::exit(2);
        }
        return;
    }
    if args.first().map(String::as_str) == Some("--check-official") {
        println!(
            "{}",
            serde_json::to_string_pretty(&cliphost_updater::check_official_release().unwrap())
                .unwrap()
        );
        return;
    }
    if args.first().map(String::as_str) == Some("--fixture-parent") {
        parent(&args[1..]);
        return;
    }
    host(&args);
}

fn parent(args: &[String]) {
    let stage = PathBuf::from(&args[0]);
    let ready = PathBuf::from(&args[1]);
    let control = PathBuf::from(&args[2]);
    for path in [&stage, ready.parent().unwrap(), control.parent().unwrap()] {
        cliphost_updater::test_support::validate_fixture_root(path).unwrap();
    }
    let config = UpdateConfig {
        current_version: "1.0.0.0".into(),
        executable: std::env::current_exe().unwrap(),
        data_root: PathBuf::from(&args[3]),
        api_port: args[4].parse().unwrap(),
        allow_install: false,
    };
    let prepared = cliphost_updater::test_support::prepare_staged(&config, &stage).unwrap();
    let service = UpdateService::new(config).unwrap();
    service.spawn_install_helper(&prepared).unwrap();
    let report = || {
        fs::write(&ready, serde_json::to_vec(&json!({"planPath":prepared.plan_path,"session":cliphost_updater::test_support::session(&prepared)})).unwrap()).unwrap();
    };
    report();
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let command = fs::read_to_string(&control).unwrap_or_default();
        match command.as_str() {
            "authorize" => {
                service.authorize_install_helper(&prepared).unwrap();
                return;
            }
            "exit-unapproved" | "stop" => return,
            "cancel" => {
                service.cancel_install_helper(&prepared).unwrap();
                fs::write(&control, "cancelled").unwrap();
            }
            "retry" => {
                service.spawn_install_helper(&prepared).unwrap();
                fs::write(&control, "retried").unwrap();
                report();
            }
            _ => {}
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("isolated fixture parent timed out");
}

fn host(args: &[String]) {
    let argument = |name| {
        args.windows(2)
            .find(|a| a[0] == name)
            .map(|a| a[1].clone())
            .unwrap()
    };
    let data = PathBuf::from(argument("--data-root"));
    cliphost_updater::test_support::validate_fixture_root(&data).unwrap();
    cliphost_updater::test_support::validate_fixture_root(
        std::env::current_exe().unwrap().parent().unwrap(),
    )
    .unwrap();
    let port: u16 = argument("--api-port").parse().unwrap();
    let manifest: Value = serde_json::from_slice(
        &fs::read(
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .join("build-info.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let version = manifest["version"].as_str().unwrap().to_string();
    if fs::read_to_string(data.join("fail-version")).unwrap_or_default() == version {
        std::process::exit(7);
    }
    fs::write(data.join("restart-argv.json"), serde_json::to_vec(&json!({"args":args,"dataEnv":std::env::var("BETTERCLIP_TAURI_DATA_ROOT").unwrap(),"pid":std::process::id()})).unwrap()).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline && !data.join("stop").exists() {
        let Ok((mut stream, _)) = listener.accept() else {
            thread::sleep(Duration::from_millis(20));
            continue;
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buffer = [0_u8; 8192];
        let length = stream.read(&mut buffer).unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..length]);
        let path = request.split_whitespace().nth(1).unwrap_or("");
        let body = match path {
            "/api/health" => json!({"ok":true,"service":"intellitools-cliphost-tauri","processId":std::process::id(),"windowRuntimeReady":true,"isDebugConfiguration":false}),
            "/api/health/app-info" => json!({"clipHost":{"fileVersion":version,"runtime":"gpui+rust"}}),
            "/api/health/data-paths" => json!({"dataRoot":data}),
            "/api/clip/chrome" => { fs::write(data.join("stop"), "normal fixture exit").unwrap(); json!({"ok":true}) }
            _ => json!({"error":"unknown fixture endpoint"}),
        }.to_string();
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
    }
}
