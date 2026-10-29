//! Opt-in cross-language test. Run scripts/Test-LanInterop.ps1 to build the C# peer.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use cliphost_core::{AppPaths, HostState};
use futures_util::FutureExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::TcpListener;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::{sleep, timeout};

use crate::{event_bus, HttpState};

struct CSharpPeer {
    child: Child,
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
}

impl CSharpPeer {
    fn start(exe: &Path, boundary: &Path, root: &Path) -> Self {
        let stderr = std::fs::File::create(root.join("csharp-stderr.log")).unwrap();
        let mut child = Command::new(exe)
            .arg(boundary)
            .arg(root.join("csharp"))
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn()
            .expect("start isolated C# LAN services");
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            input,
            output,
        }
    }

    async fn read(&mut self) -> Value {
        let line = timeout(Duration::from_secs(40), self.output.next_line())
            .await
            .expect("C# peer timed out")
            .unwrap()
            .expect("C# peer exited; see csharp-stderr.log");
        let value: Value = serde_json::from_str(&line).expect("C# peer JSON response");
        assert_eq!(value["ok"], true, "C# peer failure: {value}");
        value
    }

    async fn command(&mut self, value: Value) -> Value {
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        self.input.write_all(&line).await.unwrap();
        self.input.flush().await.unwrap();
        self.read().await
    }

    async fn shutdown(&mut self) {
        self.command(json!({"op":"exit"})).await;
        let exit = timeout(Duration::from_secs(10), self.child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exit.code(), Some(0));
    }
}

fn read_isolated_file(path: &Value, root: &Path) -> Vec<u8> {
    let path = std::fs::canonicalize(path.as_str().unwrap()).unwrap();
    assert!(
        path.starts_with(root),
        "received file escaped isolated data root"
    );
    std::fs::read(path).unwrap()
}

async fn rust_send(host: &HostState, text: &str, files: Vec<PathBuf>) -> Value {
    let host = host.clone();
    let params = json!({"conversationId":"all", "text":text, "filePaths":files});
    tokio::task::spawn_blocking(move || host.lan_send(&params))
        .await
        .unwrap()
        .unwrap()
}

async fn csharp_receipt(peer: &mut CSharpPeer, id: &Value) -> Value {
    timeout(Duration::from_secs(30), async {
        loop {
            let receipt = peer.command(json!({"op":"get", "id":id})).await;
            if receipt["messageCount"] == 1 {
                return receipt;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("Rust delivery did not reach C#")
}

fn rust_receipt(host: &HostState, id: &Value) -> Value {
    let messages = host.lan_messages(&json!({"conversationId":"all"})).unwrap();
    let matching: Vec<_> = messages
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["id"] == *id)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "missing or duplicated incoming message: {messages}"
    );
    matching[0].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires compiled C# peer; run scripts/Test-LanInterop.ps1"]
async fn csharp_rust_lan_interop() {
    cliphost_core::force_headless(true);
    std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
    let boundary = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join(".tmp")
        .canonicalize()
        .unwrap();
    let root = PathBuf::from(std::env::var_os("CLIPHOST_LAN_INTEROP_ROOT").expect("audit root"))
        .canonicalize()
        .unwrap();
    assert!(root.starts_with(&boundary) && root != boundary);
    assert!(
        !root.join("rust").exists() && !root.join("csharp").exists(),
        "use a fresh audit root"
    );
    let exe =
        PathBuf::from(std::env::var_os("CLIPHOST_LAN_INTEROP_PEER").expect("C# peer executable"))
            .canonicalize()
            .unwrap();
    assert!(exe.starts_with(&boundary));
    let mut peer = CSharpPeer::start(&exe, &boundary, &root);
    let peer_pid = peer.child.id().unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        let ready = peer.read().await;
        let csharp_port = ready["port"].as_u64().unwrap() as u16;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rust_port = listener.local_addr().unwrap().port();
        let host = HostState::open(AppPaths::from_data_root(root.join("rust")), 28965).unwrap();
        host.lan_config_update(
            &json!({"enabled":true, "port":rust_port, "deviceAlias":"Rust interop"}),
        )
        .unwrap();
        let server = tokio::spawn(crate::lan_tls::serve_lan_https_listener(
            HttpState {
                host: host.clone(),
                events: event_bus(),
            },
            listener,
        ));
        timeout(Duration::from_secs(10), async {
            while host.lan_status()["running"] != true {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let rust_identity = host.lan_status();

        // Feed the production discovery parser an announcement without opening UDP adapters.
        let announcement = json!({"protocol":"cea-clip-link", "version":1, "kind":"announce",
        "deviceId":ready["deviceId"], "alias":ready["alias"], "port":csharp_port,
        "certificateFingerprint":ready["certificateFingerprint"], "pairingAvailable":true});
        host.lan_ingest_datagram(&serde_json::to_vec(&announcement).unwrap(), "127.0.0.1");
        let pair_host = host.clone();
        let pair_params = json!({"deviceId":ready["deviceId"], "pairingCode":ready["pairingCode"]});
        let pair = tokio::task::spawn_blocking(move || pair_host.lan_pair(&pair_params))
            .await
            .unwrap();
        let pair = pair.expect("Rust -> C# pairing; see csharp-stderr.log");
        assert_eq!(pair["paired"], true);

        let text = "Rust → C#：中文 🧪\r\nsecond line\t尾行";
        let sent_text = rust_send(&host, text, vec![]).await;
        let received_text = csharp_receipt(&mut peer, &sent_text["id"]).await;
        assert_eq!(received_text["message"]["text"], text);
        assert_eq!(received_text["message"]["direction"], "incoming");
        assert!(received_text["message"]["attachments"]
            .as_array()
            .unwrap()
            .is_empty());

        let payload: Vec<u8> = (0..262177).map(|i| (i % 253) as u8).collect();
        let file = root.join("来自 Rust 🧪.bin");
        std::fs::write(&file, &payload).unwrap();
        let sent_file = rust_send(&host, "Rust file", vec![file]).await;
        let received_file = csharp_receipt(&mut peer, &sent_file["id"]).await;
        let attachment = &received_file["message"]["attachments"][0];
        assert_eq!(attachment["name"], "来自 Rust 🧪.bin");
        assert_eq!(attachment["sizeBytes"], payload.len());
        assert_eq!(read_isolated_file(&attachment["localPath"], &root), payload);

        // Reverse pairing exercises C#'s real client and Rust's receiver, rotating both tokens.
        let paired = peer
            .command(json!({"op":"pair", "deviceId":rust_identity["deviceId"],
        "port":rust_port, "certificateFingerprint":rust_identity["certificateFingerprint"],
        "pairingCode":rust_identity["pairingCode"]["code"]}))
            .await;
        assert_eq!(paired["deviceId"], rust_identity["deviceId"]);
        let text = "C# → Rust：中文 🧪\r\nsecond line\t尾行";
        let sent = peer
            .command(
                json!({"op":"send", "peerId":rust_identity["deviceId"], "text":text, "file":false}),
            )
            .await;
        assert_eq!(sent["delivered"], true);
        let received = rust_receipt(&host, &sent["id"]);
        assert_eq!(received["text"], text);
        assert_eq!(received["direction"], "incoming");
        let replay = peer.command(json!({"op":"replay"})).await;
        assert_eq!(replay["delivered"], true);
        assert_eq!(rust_receipt(&host, &sent["id"]), received);

        let sent = peer
        .command(
            json!({"op":"send", "peerId":rust_identity["deviceId"], "text":"C# file", "file":true}),
        )
        .await;
        assert_eq!(sent["delivered"], true);
        let received = rust_receipt(&host, &sent["id"]);
        let attachment = &received["attachments"][0];
        assert_eq!(attachment["name"], "来自 CSharp 🧪.bin");
        assert_eq!(attachment["sha256"], sent["attachments"][0]["sha256"]);
        assert_eq!(attachment["sizeBytes"], 131123);
        let expected: Vec<u8> = (0..131123).map(|i| (i % 251) as u8).collect();
        assert_eq!(
            read_isolated_file(&attachment["localPath"], &root),
            expected
        );
        let replay = peer.command(json!({"op":"replay"})).await;
        assert_eq!(replay["delivered"], true);
        assert_eq!(
            replay["transferred"], 0,
            "replay unnecessarily uploaded the file"
        );
        assert_eq!(rust_receipt(&host, &sent["id"]), received);
        let probe = peer
            .command(json!({"op":"probeWrongCertificate", "peerId":rust_identity["deviceId"]}))
            .await;
        assert_eq!(probe["rejected"], true);

        // Re-pairing must also leave Rust's sender using the new token.
        let sent = rust_send(&host, "after token rotation", vec![]).await;
        assert_eq!(
            csharp_receipt(&mut peer, &sent["id"]).await["message"]["text"],
            "after token rotation"
        );
        (server, rust_port, csharp_port)
    })
    .catch_unwind()
    .await;
    // Unwind the C# certificate and network resources even if a Rust assertion fails.
    if peer.child.try_wait().unwrap().is_none() {
        peer.shutdown().await;
    }
    let key_record = root.join("csharp/key-lifetime.json");
    if key_record.exists() {
        let key: Value = serde_json::from_slice(&std::fs::read(key_record).unwrap()).unwrap();
        assert_eq!(key["existed"], true);
        assert!(
            !Path::new(key["keyPath"].as_str().unwrap()).exists(),
            "C# private key leaked"
        );
    }
    let (server, rust_port, csharp_port) = match result {
        Ok(value) => value,
        Err(error) => std::panic::resume_unwind(error),
    };
    server.abort();
    let _ = server.await;
    for port in [rust_port, csharp_port] {
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .expect("test listener leaked");
    }
    std::fs::write(root.join("acceptance.json"), serde_json::to_vec_pretty(&json!({
        "ok":true, "transport":"HTTPS on 127.0.0.1", "csharpPeerPid":peer_pid, "csharpExitCode":0,
        "bothPairingDirections":true, "bidirectionalUnicodeText":true, "bidirectionalBinaryFiles":true,
        "duplicateTextAndFileIdempotent":true, "fileReplayBytes":0, "wrongCertificateRejected":true,
        "sendAfterTokenRotation":true, "testListenersReleased":true, "desktopInputUsed":false,
        "physicalTwoPcLanVerified":false, "csharpTemporaryKeyRemoved":true
    })).unwrap()).unwrap();
}
