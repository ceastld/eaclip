#![cfg(feature = "test-support")]
use cliphost_updater::{test_support, OfficialRelease};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};

mod common;

fn release(address: String, body: &[u8]) -> OfficialRelease {
    OfficialRelease {
        version: "2.0.0.0".into(),
        download_urls: vec![address],
        sha256: format!("{:x}", Sha256::digest(body)),
        size: Some(body.len() as u64),
        release_url: "http://127.0.0.1/simulation-only".into(),
        release_notes: None,
        source: "isolated loopback simulation".into(),
    }
}

fn fake_pe(marker: u8) -> Vec<u8> {
    let mut bytes = vec![marker; 128];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[60..64].copy_from_slice(&64_u32.to_le_bytes());
    bytes[64..68].copy_from_slice(b"PE\0\0");
    bytes[68..70].copy_from_slice(&0x8664_u16.to_le_bytes());
    bytes[88..90].copy_from_slice(&0x20b_u16.to_le_bytes());
    bytes
}

#[test]
fn loopback_download_verifies_outer_sha_inner_payload_and_rejects_bad_responses() {
    let root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/updater-download-tests");
    fs::create_dir_all(&root).unwrap();
    let temp = tempfile::Builder::new()
        .prefix("download-")
        .tempdir_in(&root)
        .unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let mut files = Vec::new();
    for name in ["cliphost-app.exe", "vcruntime140.dll"] {
        let bytes = fake_pe(1);
        fs::write(source.join(name), &bytes).unwrap();
        files.push(json!({"name":name,"sha256":format!("{:x}",Sha256::digest(&bytes)),"sizeBytes":bytes.len()}));
    }
    fs::write(source.join("build-info.json"), serde_json::to_vec(&json!({"product":"ClipHost Rust","version":"2.0.0.0","executable":"cliphost-app.exe","sha256":files[0]["sha256"],"renderer":"gpui","profile":"release","architecture":"x64","target":"x86_64-pc-windows-msvc","files":files})).unwrap()).unwrap();
    fs::write(source.join("README.txt"), "download fixture").unwrap();
    fs::write(source.join("SHA256SUMS.txt"), "download fixture").unwrap();
    let body = common::archive_directory(&source);
    fs::write(
        temp.path().join("old-program.exe"),
        b"untouched old executable",
    )
    .unwrap();
    fs::write(temp.path().join("userdata.db"), b"untouched old database").unwrap();
    let mut reports = Vec::new();
    for mode in [
        "valid",
        "wrong-sha",
        "truncated",
        "oversized-header",
        "redirect",
    ] {
        let length = match mode {
            "truncated" => Some(body.len() as u64 + 1),
            "oversized-header" => Some(512 * 1024 * 1024 + 1),
            _ => None,
        };
        let (address, server) = common::serve_once(body.clone(), length, mode == "redirect");
        let mut metadata = release(address, &body);
        if mode == "wrong-sha" {
            metadata.sha256 = "0".repeat(64);
        }
        if matches!(mode, "truncated" | "oversized-header") {
            metadata.size = None;
        }
        let destination = temp.path().join(format!("{mode}.zip"));
        let mut progress = Vec::new();
        let result = test_support::download_loopback(&metadata, &destination, |done, total| {
            progress.push(json!({"downloadedBytes":done,"totalBytes":total}))
        });
        server.join().unwrap();
        if mode == "valid" {
            result.as_ref().unwrap();
            assert_eq!(fs::read(&destination).unwrap(), body);
            test_support::extract_downloaded(
                &destination,
                &temp.path().join("verified-payload"),
                "2.0.0.0",
            )
            .unwrap();
            assert_eq!(
                progress.last().unwrap()["downloadedBytes"],
                body.len() as u64
            );
        } else {
            assert!(result.is_err(), "{mode}");
            assert!(
                !destination.exists(),
                "rejected download must be removed: {mode}"
            );
        }
        reports.push(json!({"mode":mode,"passed":true,"accepted":result.is_ok(),"error":result.err().map(|e|e.to_string()),"progress":progress}));
        assert_eq!(
            fs::read(temp.path().join("old-program.exe")).unwrap(),
            b"untouched old executable"
        );
        assert_eq!(
            fs::read(temp.path().join("userdata.db")).unwrap(),
            b"untouched old database"
        );
    }
    // A valid outer digest cannot hide an inner executable whose hash differs
    // from build-info. No installation/helper starts after this rejection.
    fs::write(source.join("cliphost-app.exe"), fake_pe(2)).unwrap();
    let tampered = common::archive_directory(&source);
    let (address, server) = common::serve_once(tampered.clone(), None, false);
    let destination = temp.path().join("tampered-payload.zip");
    test_support::download_loopback(&release(address, &tampered), &destination, |_, _| {}).unwrap();
    server.join().unwrap();
    let error = test_support::extract_downloaded(
        &destination,
        &temp.path().join("tampered-payload"),
        "2.0.0.0",
    )
    .unwrap_err();
    reports.push(json!({"mode":"inner-payload-hash-mismatch","passed":true,"downloadDigestAccepted":true,"extractRejected":true,"error":error.to_string()}));
    for address in [
        "http://localhost:9/file",
        "http://192.0.2.1:9/file",
        "http://127.0.0.2:9/file",
        "https://127.0.0.1:9/file",
    ] {
        let error = test_support::download_loopback(
            &release(address.into(), &body),
            &temp.path().join("forbidden.zip"),
            |_, _| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("只允许127.0.0.1或::1"));
        reports.push(json!({"mode":"forbidden-transport","url":address,"passed":true,"error":error.to_string()}));
    }
    assert_eq!(
        fs::read(temp.path().join("old-program.exe")).unwrap(),
        b"untouched old executable"
    );
    assert_eq!(
        fs::read(temp.path().join("userdata.db")).unwrap(),
        b"untouched old database"
    );
    if let Some(directory) = common::report_directory() {
        fs::write(
            directory.join("download-scenarios.json"),
            serde_json::to_vec_pretty(&reports).unwrap(),
        )
        .unwrap();
    }
}
