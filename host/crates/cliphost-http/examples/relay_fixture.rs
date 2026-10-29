//! Synthetic public-relay fixture. Uses the production worker, an explicit .tmp
//! database and simulated clipboard. It never opens a local HTTP/LAN listener.
use cliphost_core::{clipboard::ClipboardSnapshot, AppPaths, HostState};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
#[tokio::main]
async fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| {
        args.iter()
            .position(|s| s == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let root = PathBuf::from(value("--data-root").ok_or("--data-root is required")?);
    if !root.is_absolute() || !root.components().any(|c| c.as_os_str() == ".tmp") {
        return Err("Use an absolute .tmp data root.".into());
    }
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    cliphost_core::force_headless(true);
    let host = HostState::open(AppPaths::from_data_root(root.clone()), 29065)?;
    if args.iter().any(|s| s == "--status") {
        println!("{}", host.relay_status());
        return Ok(());
    }
    let invitation = PathBuf::from(value("--invitation").ok_or("--invitation is required")?);
    let bytes = std::fs::read(invitation).map_err(|_| "Cannot read invitation.")?;
    if bytes.len() > 16 * 1024 {
        return Err("Invitation oversized.".into());
    }
    let invitation: Value =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid invitation JSON.")?;
    host.relay_configure(&json!({"invitation":invitation}))?;
    host.relay_configure(&json!({"enabled":true,"sendEnabled":true,"receiveEnabled":true}))?;
    cliphost_http::start_relay_fixture(host.clone())?;
    let seconds = value("--seconds")
        .unwrap_or("120".into())
        .parse::<u64>()
        .map_err(|_| "Invalid --seconds")?
        .min(600);
    let until = Instant::now() + Duration::from_secs(seconds);
    let mut sent_text = None;
    let mut sent_image = None;
    loop {
        if Instant::now() >= until {
            break;
        }
        for (name, is_image, last) in [
            ("outgoing-text.txt", false, &mut sent_text),
            ("outgoing-image.png", true, &mut sent_image),
        ] {
            let path = root.join(name);
            if let Ok(bytes) = std::fs::read(&path) {
                let hash = cliphost_core::model::sha256_hex(&bytes);
                if last.as_ref() != Some(&hash) {
                    let snapshot = if is_image {
                        ClipboardSnapshot {
                            png: Some(bytes),
                            ..Default::default()
                        }
                    } else {
                        ClipboardSnapshot {
                            text: Some(
                                String::from_utf8(bytes)
                                    .map_err(|_| "Fixture text must be UTF-8")?,
                            ),
                            ..Default::default()
                        }
                    };
                    host.relay_fixture_copy(&snapshot)?;
                    *last = Some(hash);
                }
            }
        }
        let records = host.relay_fixture_records()?;
        let summary:Vec<Value>=records.iter().map(|r|json!({"id":r.id,"kind":r.kind,"textSha256":cliphost_core::model::sha256_hex(r.body_text.as_deref().unwrap_or("").as_bytes()),"contentHash":r.content_hash})).collect();
        let status = json!({"relay":host.relay_status(),"lanEnabled":host.lan_status()["enabled"],"received":summary,"sentTextSha256":sent_text,"sentImageSha256":sent_image});
        std::fs::write(
            root.join("status.json"),
            serde_json::to_vec_pretty(&status).unwrap(),
        )
        .map_err(|e| e.to_string())?;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    host.relay_configure(&json!({"enabled":false}))?;
    println!("{}", host.relay_status());
    Ok(())
}
