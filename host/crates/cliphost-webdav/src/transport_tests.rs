use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

#[derive(Clone, Default)]
enum Mode {
    #[default]
    Normal,
    Status(u16),
    Redirect(String),
    WeakTag,
    MissingTag,
    MultipleTags,
    DeclaredOversize,
    ChunkedOversize,
    Truncated,
    Encoded,
    Slow,
    IgnoreCreate,
    IgnoreReplace,
    DropPutOnce,
    BogusCollection,
}

#[derive(Clone, Debug)]
struct RequestLog {
    method: String,
    path: String,
    if_none_match: Option<String>,
    if_match: Option<String>,
}

struct State {
    files: BTreeMap<String, (Vec<u8>, u64)>,
    directories: BTreeSet<String>,
    revision: u64,
    requests: Vec<RequestLog>,
    mode: Mode,
}

struct DavFixture {
    address: SocketAddr,
    state: Arc<Mutex<State>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl DavFixture {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(Mutex::new(State {
            files: BTreeMap::new(),
            directories: BTreeSet::from(["/dav/user".to_owned()]),
            revision: 0,
            requests: Vec::new(),
            mode: Mode::Normal,
        }));
        let stopped = Arc::new(AtomicBool::new(false));
        let server_state = state.clone();
        let server_stopped = stopped.clone();
        let worker = thread::spawn(move || {
            // Each DAV operation is processed atomically. Concurrent clients
            // still connect independently and compete on the same revision.
            while !server_stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Accepted sockets can inherit a nonblocking Windows
                        // listener. A read racing the arriving request must
                        // wait for bytes, not abort with WouldBlock/reset.
                        stream.set_nonblocking(false).unwrap();
                        handle_request(&mut stream, &server_state);
                        // Gracefully send queued response bytes before closing.
                        // Windows closesocket can otherwise race those bytes
                        // with a reset, producing an artificial transport fault.
                        let _ = stream.shutdown(std::net::Shutdown::Write);
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                        let mut drain = [0u8; 1024];
                        loop {
                            match stream.read(&mut drain) {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            state,
            stopped,
            worker: Some(worker),
        }
    }

    fn config(&self) -> TransportConfig {
        let mut config = TransportConfig::new(
            format!("http://{}/dav/user/", self.address),
            "test-user",
            "test-password",
        );
        config.allow_loopback_http = true;
        config.timeout = Duration::from_secs(2);
        config.connect_timeout = Duration::from_secs(1);
        config
    }

    fn store(&self) -> WebDavStore {
        WebDavStore::new(self.config()).unwrap()
    }

    fn mode(&self, mode: Mode) {
        self.state.lock().unwrap().mode = mode;
    }

    fn logs(&self) -> Vec<RequestLog> {
        self.state.lock().unwrap().requests.clone()
    }
}

impl Drop for DavFixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn handle_request(mut stream: &mut TcpStream, state: &Arc<Mutex<State>>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(&mut *stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    let parts: Vec<_> = line.split_whitespace().collect();
    if parts.len() != 3 {
        return;
    }
    let method = parts[0].to_owned();
    let path = parts[1].to_owned();
    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).is_err() || line.len() > 8192 {
            return;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
        if headers.len() > 64 {
            return;
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if length > 1024 * 1024 {
        return;
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    drop(reader);
    let mut state = state.lock().unwrap();
    state.requests.push(RequestLog {
        method: method.clone(),
        path: path.clone(),
        if_none_match: headers.get("if-none-match").cloned(),
        if_match: headers.get("if-match").cloned(),
    });
    let path = path.trim_end_matches('/').to_owned();
    let mode = state.mode.clone();
    if let Mode::Status(status) = mode {
        reply(&mut stream, status, &[], b"private server error body");
        return;
    }
    if let Mode::Redirect(ref location) = mode {
        reply(&mut stream, 302, &[("Location", location)], b"");
        return;
    }
    match method.as_str() {
        "PROPFIND" => {
            if !state.directories.contains(&path) && !state.files.contains_key(&path) {
                reply(&mut stream, 404, &[], b"");
                return;
            }
            assert_eq!(headers.get("depth").map(String::as_str), Some("0"));
            let resource_type = if matches!(mode, Mode::BogusCollection) {
                "<x:collection xmlns:x=\"not-DAV\"/>"
            } else if state.directories.contains(&path) {
                "<d:collection/>"
            } else {
                ""
            };
            let body = format!("<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>{path}/</d:href><d:propstat><d:prop><d:resourcetype>{resource_type}</d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>");
            reply(
                &mut stream,
                207,
                &[("Content-Type", "application/xml")],
                body.as_bytes(),
            );
        }
        "MKCOL" => {
            let parent = path
                .rsplit_once('/')
                .map(|(parent, _)| parent)
                .unwrap_or("");
            let status = if state.directories.contains(&path) || state.files.contains_key(&path) {
                405
            } else if !state.directories.contains(parent) {
                409
            } else {
                state.directories.insert(path);
                201
            };
            reply(&mut stream, status, &[], b"");
        }
        "PUT" => {
            let parent = path
                .rsplit_once('/')
                .map(|(parent, _)| parent)
                .unwrap_or("");
            if !state.directories.contains(parent) {
                reply(&mut stream, 409, &[], b"");
                return;
            }
            let exists = state.files.get(&path);
            let current_tag = exists.map(|(_, revision)| format!("\"v{revision}\""));
            let create_conflict = headers.get("if-none-match").is_some_and(|v| v == "*")
                && exists.is_some()
                && !matches!(mode, Mode::IgnoreCreate);
            let replace_conflict = headers
                .get("if-match")
                .is_some_and(|tag| current_tag.as_ref() != Some(tag))
                && !matches!(mode, Mode::IgnoreReplace);
            if create_conflict || replace_conflict {
                reply(&mut stream, 412, &[], b"");
                return;
            }
            state.revision += 1;
            let revision = state.revision;
            state.files.insert(path, (body, revision));
            if matches!(mode, Mode::DropPutOnce) {
                state.mode = Mode::Normal;
                // The write committed, but its successful reply is lost.
                return;
            }
            reply(&mut stream, 201, &[], b"");
        }
        "GET" => {
            if matches!(mode, Mode::Slow) {
                thread::sleep(Duration::from_millis(200));
            }
            let Some((body, revision)) = state.files.get(&path) else {
                reply(&mut stream, 404, &[], b"");
                return;
            };
            let tag = format!("\"v{revision}\"");
            match mode {
                Mode::WeakTag => reply(&mut stream, 200, &[("ETag", "W/\"weak\"")], body),
                Mode::MissingTag => reply(&mut stream, 200, &[], body),
                Mode::MultipleTags => reply(
                    &mut stream,
                    200,
                    &[("ETag", &tag), ("ETag", "\"other\"")],
                    body,
                ),
                Mode::DeclaredOversize => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n",
                    );
                }
                Mode::ChunkedOversize => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n21\r\n012345678901234567890123456789012\r\n0\r\n\r\n",
                    );
                }
                Mode::Truncated => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: 10\r\nConnection: close\r\n\r\nshort",
                    );
                }
                Mode::Encoded => reply(
                    &mut stream,
                    200,
                    &[("ETag", &tag), ("Content-Encoding", "gzip")],
                    body,
                ),
                _ => reply(&mut stream, 200, &[("ETag", &tag)], body),
            }
        }
        "DELETE" => {
            state
                .files
                .retain(|key, _| !key.starts_with(&format!("{path}/")) && key != &path);
            state
                .directories
                .retain(|key| !key.starts_with(&format!("{path}/")) && key != &path);
            reply(&mut stream, 204, &[], b"");
        }
        _ => reply(&mut stream, 405, &[], b""),
    }
}

fn reply(stream: &mut TcpStream, status: u16, headers: &[(&str, &str)], body: &[u8]) {
    let mut response = format!(
        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (key, value) in headers {
        response.push_str(&format!("{key}: {value}\r\n"));
    }
    response.push_str("\r\n");
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(body);
}

fn expect_transport<T>(result: Result<T, SyncError>, expected: &'static str) {
    match result {
        Err(SyncError::Transport(actual)) => assert_eq!(actual, expected),
        Err(_) => panic!("unexpected non-transport error"),
        Ok(_) => panic!("expected a transport error"),
    }
}

#[test]
fn endpoint_policy_rejects_credentials_and_normalized_traversal() {
    for endpoint in [
        "http://example.test/dav",
        "https://user:password@example.test/dav",
        "https://@example.test/dav",
        "https://example.test/dav?secret=value",
        "https://example.test/dav#secret",
        "https://example.test/a/../dav",
        "https://example.test/a/%2e%2e/dav",
        "https://example.test/a/%252e%252e/dav",
        "https://example.test/a%2fb/dav",
        "https://example.test/a%5cb/dav",
        "https://example.test/a//dav",
        "https://example.test/a\\dav",
    ] {
        assert!(WebDavStore::new(TransportConfig::new(endpoint, "user", "password")).is_err());
    }
    for endpoint in ["http://localhost/dav", "http://192.0.2.1/dav"] {
        let mut config = TransportConfig::new(endpoint, "user", "password");
        config.allow_loopback_http = true;
        expect_transport(WebDavStore::new(config), "https_required");
    }
    assert!(WebDavStore::new(TransportConfig::new(
        "https://example.test/dav/%E6%94%B6%E8%97%8F%20files/",
        "user",
        "password"
    ))
    .is_ok());
}

#[test]
fn credentials_and_endpoint_are_absent_from_debug_and_errors() {
    let config = TransportConfig::new(
        "https://example.test/private-root-should-not-be-logged/",
        "private-user-should-not-be-logged",
        "private-password-should-not-be-logged",
    );
    let debug = format!("{config:?}");
    assert!(!debug.contains("should-not-be-logged"));
    let store = WebDavStore::new(config).unwrap();
    assert!(!format!("{store:?}").contains("should-not-be-logged"));
    let error = store
        .get("../private-object-should-not-be-logged")
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains("should-not-be-logged"));
}

#[test]
fn object_paths_cannot_escape_the_root_and_missing_get_does_not_initialize() {
    let server = DavFixture::start();
    let store = server.store();
    for path in [
        "",
        "/absolute",
        "../escape",
        "a/../escape",
        "a//b",
        "a/%2e%2e/b",
        "https://example.test/a",
        "a\\b",
        "a?token=x",
        "a#b",
        "a/",
    ] {
        expect_transport(store.get(path), "invalid_object_path");
    }
    assert!(server.logs().is_empty());
    assert!(store.get("missing.json").unwrap().is_none());
    let logs = server.logs();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].method, "GET");
    assert_eq!(logs[0].path, "/dav/user/missing.json");
    assert_eq!(server.state.lock().unwrap().directories.len(), 1);
}

#[test]
fn real_http_create_replace_keep_conditions_and_make_only_child_collections() {
    let server = DavFixture::start();
    let store = server.store();
    assert!(store
        .create("objects/aa/object.bin", b"first payload")
        .unwrap());
    assert!(!store
        .create("objects/aa/object.bin", b"must not overwrite")
        .unwrap());
    let first = store.get("objects/aa/object.bin").unwrap().unwrap();
    assert_eq!(first.bytes, b"first payload");
    assert!(!store
        .replace("objects/aa/object.bin", "\"wrong\"", b"bad")
        .unwrap());
    assert!(store
        .replace("objects/aa/object.bin", &first.etag, b"replacement")
        .unwrap());
    assert!(!store
        .replace("objects/aa/object.bin", &first.etag, b"stale")
        .unwrap());
    let last = store.get("objects/aa/object.bin").unwrap().unwrap();
    assert_eq!(last.bytes, b"replacement");
    assert_ne!(first.etag, last.etag);
    let logs = server.logs();
    assert!(logs
        .iter()
        .filter(|r| r.method == "MKCOL")
        .all(|r| { r.path == "/dav/user/objects/" || r.path == "/dav/user/objects/aa/" }));
    assert!(logs
        .iter()
        .filter(|r| r.method == "PUT" && r.if_none_match.is_some())
        .all(|r| r.if_none_match.as_deref() == Some("*")));
    assert!(logs
        .iter()
        .any(|r| r.if_match.as_deref() == Some(&first.etag)));
}

#[test]
fn probe_races_conditions_checks_full_readback_and_cleans_only_its_random_child() {
    let server = DavFixture::start();
    let store = server.store();
    assert!(store.create("keep.bin", b"user data").unwrap());
    let report = store.probe().unwrap();
    assert!(report.conditional_create && report.conditional_replace);
    assert!(report.strong_etags && report.full_readback);
    let state = server.state.lock().unwrap();
    assert_eq!(state.files.len(), 1);
    assert_eq!(state.files["/dav/user/keep.bin"].0, b"user data");
    assert_eq!(state.directories, BTreeSet::from(["/dav/user".to_owned()]));
    assert!(state
        .requests
        .iter()
        .filter(|r| r.method == "DELETE")
        .all(|r| r.path.starts_with("/dav/user/.cliphost-probe-")));
}

#[test]
fn explicit_probe_creates_only_the_selected_missing_dedicated_root() {
    let server = DavFixture::start();
    let mut config = server.config();
    config.endpoint = format!("http://{}/dav/user/ClipHost/", server.address);
    let store = WebDavStore::new(config).unwrap();
    assert!(store.get("vault.json").unwrap().is_none());
    assert!(server.logs().iter().all(|log| log.method == "GET"));
    store.probe().unwrap();
    let state = server.state.lock().unwrap();
    assert_eq!(
        state.directories,
        BTreeSet::from(["/dav/user".to_owned(), "/dav/user/ClipHost".to_owned(),])
    );
    assert!(state.files.is_empty());
    assert!(state
        .requests
        .iter()
        .filter(|log| log.method == "MKCOL")
        .all(|log| log.path.starts_with("/dav/user/ClipHost")));
    assert!(state
        .requests
        .iter()
        .filter(|log| log.method == "DELETE")
        .all(|log| log.path.starts_with("/dav/user/ClipHost/.cliphost-probe-")));
}

#[test]
fn missing_parent_or_a_file_in_place_of_a_collection_is_never_accepted() {
    let server = DavFixture::start();
    let mut config = server.config();
    config.endpoint = format!("http://{}/missing-parent/ClipHost/", server.address);
    let store = WebDavStore::new(config).unwrap();
    expect_transport(store.ensure_root(), "missing_parent");
    assert!(server
        .logs()
        .iter()
        .filter(|log| log.method == "MKCOL")
        .all(|log| log.path == "/missing-parent/ClipHost/"));
    server.state.lock().unwrap().files.insert(
        "/dav/user/file-as-directory".to_owned(),
        (b"preserve this file".to_vec(), 1),
    );
    let store = server.store();
    expect_transport(
        store.create("file-as-directory/object.bin", b"data"),
        "not_a_collection",
    );
    assert_eq!(
        server.state.lock().unwrap().files["/dav/user/file-as-directory"].0,
        b"preserve this file"
    );
    assert!(!server.logs().iter().any(|log| log.method == "PUT"));
    server.mode(Mode::BogusCollection);
    expect_transport(store.ensure_root(), "not_a_collection");
}

#[test]
fn probe_rejects_servers_ignoring_either_conditional_write() {
    for (mode, error) in [
        (Mode::IgnoreCreate, "conditional_create_unsupported"),
        (Mode::IgnoreReplace, "conditional_replace_unsupported"),
        (Mode::WeakTag, "weak_or_invalid_etag"),
    ] {
        let server = DavFixture::start();
        server.mode(mode);
        expect_transport(server.store().probe(), error);
        assert!(server.state.lock().unwrap().files.is_empty());
    }
}

#[test]
fn strong_etag_is_required_for_read_and_replace() {
    let server = DavFixture::start();
    let store = server.store();
    assert!(store.create("object.bin", b"body").unwrap());
    for mode in [Mode::WeakTag, Mode::MissingTag, Mode::MultipleTags] {
        server.mode(mode);
        expect_transport(store.get("object.bin"), "weak_or_invalid_etag");
    }
    let before = server.logs().len();
    for tag in [
        "W/\"weak\"",
        "unquoted",
        "*",
        "\"one\", \"two\"",
        "\"bad\r\n\"",
    ] {
        expect_transport(
            store.replace("object.bin", tag, b"body"),
            "weak_or_invalid_etag",
        );
    }
    assert_eq!(before, server.logs().len());
    assert!(is_strong_etag("\"\""));
    assert!(is_strong_etag("\"opaque\\tag\""));
}

#[test]
fn response_limit_applies_to_declared_and_chunked_bodies_and_truncation_fails() {
    let server = DavFixture::start();
    let mut config = server.config();
    config.max_response_bytes = 32;
    let store = WebDavStore::new(config).unwrap();
    assert!(store.create("object.bin", b"body").unwrap());
    for mode in [Mode::DeclaredOversize, Mode::ChunkedOversize] {
        server.mode(mode);
        expect_transport(store.get("object.bin"), "response_too_large");
    }
    server.mode(Mode::Truncated);
    expect_transport(store.get("object.bin"), "response_read_failed");
    server.mode(Mode::Encoded);
    expect_transport(store.get("object.bin"), "unexpected_content_encoding");
    let before = server.logs().len();
    expect_transport(
        store.create("large/file.bin", &[0; 33]),
        "request_too_large",
    );
    assert_eq!(before, server.logs().len());
}

#[test]
fn redirects_are_never_followed_and_http_errors_are_not_missing_or_conflicts() {
    let source = DavFixture::start();
    let target = DavFixture::start();
    source.mode(Mode::Redirect(format!(
        "http://{}/dav/user/stolen",
        target.address
    )));
    let store = source.store();
    expect_transport(store.get("object.bin"), "redirect_rejected");
    assert!(target.logs().is_empty());
    for (status, classification) in [
        (401, "authentication_failed"),
        (403, "authentication_failed"),
        (423, "resource_locked"),
        (429, "rate_limited"),
        (500, "server_error"),
        (503, "server_error"),
        (507, "remote_storage_full"),
    ] {
        source.mode(Mode::Status(status));
        expect_transport(store.get("object.bin"), classification);
        expect_transport(store.create("object.bin", b"x"), classification);
        expect_transport(store.replace("object.bin", "\"tag\"", b"x"), classification);
    }
}

#[test]
fn request_timeout_is_bounded_and_lost_ack_does_not_retry_a_committed_put() {
    let server = DavFixture::start();
    let store = server.store();
    server.mode(Mode::DropPutOnce);
    assert!(store
        .create("object.bin", b"committed without reply")
        .is_err());
    let stored = store.get("object.bin").unwrap().unwrap();
    assert_eq!(stored.bytes, b"committed without reply");
    assert_eq!(
        server.logs().iter().filter(|r| r.method == "PUT").count(),
        1
    );
    let mut config = server.config();
    config.timeout = Duration::from_millis(40);
    config.connect_timeout = Duration::from_millis(20);
    let short_timeout = WebDavStore::new(config).unwrap();
    server.mode(Mode::Slow);
    let started = std::time::Instant::now();
    expect_transport(short_timeout.get("object.bin"), "timeout");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn two_engines_over_real_http_merge_offline_edits_deletion_and_original_attachment() {
    use crate::{blob_digest, Engine, RecoveryKey, Snapshot};
    use serde_json::json;

    let server = DavFixture::start();
    let remote = server.store();
    let temp = tempfile::tempdir().unwrap();
    let key = RecoveryKey::generate().unwrap();
    let mut a = Engine::open(&temp.path().join("device-a"), key.clone()).unwrap();
    a.create(&remote).unwrap();
    let original = b"original\0attachment\xffbytes".to_vec();
    let source = Snapshot {
        entries: BTreeMap::from([
            (
                "favor/a".into(),
                json!({"name":"original-a","unknown":"private-future-field-marker"}),
            ),
            ("favor/b".into(), json!({"name":"original-b"})),
            ("favor/deleted".into(), json!({"name":"remove-me"})),
            ("roots".into(), json!(["a", "b", "deleted"])),
        ]),
        blobs: BTreeMap::from([(blob_digest(&original), original)]),
    };
    let initial = a.prepare(&remote, &source).unwrap();
    a.acknowledge(&initial.token).unwrap();
    let mut b = Engine::open(&temp.path().join("device-b"), key).unwrap();
    b.join(&remote).unwrap();
    let received = b.prepare(&remote, &Snapshot::default()).unwrap();
    assert_eq!(received.snapshot, source);
    b.acknowledge(&received.token).unwrap();

    let mut edit_a = source.clone();
    edit_a.entries.get_mut("favor/a").unwrap()["name"] = json!("A edited offline");
    edit_a.entries.remove("favor/deleted");
    edit_a.entries.insert("roots".into(), json!(["a", "b"]));
    let published_a = a.prepare(&remote, &edit_a).unwrap();
    a.acknowledge(&published_a.token).unwrap();
    let mut edit_b = source.clone();
    edit_b.entries.get_mut("favor/b").unwrap()["name"] = json!("B edited offline");
    let merged = b.prepare(&remote, &edit_b).unwrap();
    assert!(merged.conflicts.is_empty());
    assert_eq!(
        merged.snapshot.entries["favor/a"]["name"],
        "A edited offline"
    );
    assert_eq!(
        merged.snapshot.entries["favor/b"]["name"],
        "B edited offline"
    );
    assert!(!merged.snapshot.entries.contains_key("favor/deleted"));
    assert_eq!(merged.snapshot.entries["roots"], json!(["a", "b"]));
    assert_eq!(merged.snapshot.blobs, source.blobs);
    b.acknowledge(&merged.token).unwrap();
    let arrived = a.prepare(&remote, &edit_a).unwrap();
    assert_eq!(arrived.snapshot, merged.snapshot);
    a.acknowledge(&arrived.token).unwrap();
    let unchanged = a.prepare(&remote, &arrived.snapshot).unwrap();
    assert!(!unchanged.report.published);
    assert_eq!(unchanged.report.uploaded_objects, 0);
    a.acknowledge(&unchanged.token).unwrap();
    assert!(server
        .state
        .lock()
        .unwrap()
        .files
        .values()
        .all(|(bytes, _)| {
            !String::from_utf8_lossy(bytes).contains("private-future-field-marker")
        }));
}

#[test]
fn initialized_engine_over_real_http_refuses_a_wiped_remote_without_writing() {
    use crate::{Engine, RecoveryKey, Snapshot};
    use serde_json::json;

    let server = DavFixture::start();
    let remote = server.store();
    let temp = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(temp.path(), RecoveryKey::generate().unwrap()).unwrap();
    engine.create(&remote).unwrap();
    let source = Snapshot {
        entries: BTreeMap::from([("favor/keep".into(), json!({"name":"local data"}))]),
        blobs: BTreeMap::new(),
    };
    let prepared = engine.prepare(&remote, &source).unwrap();
    engine.acknowledge(&prepared.token).unwrap();
    server.state.lock().unwrap().files.clear();
    let before = server.logs().len();
    assert!(matches!(
        engine.prepare(&remote, &source),
        Err(SyncError::MissingRemote)
    ));
    assert!(server.state.lock().unwrap().files.is_empty());
    assert!(server.logs()[before..]
        .iter()
        .all(|request| request.method == "GET"));
    assert!(!engine.has_pending_apply());
}
