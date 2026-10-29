use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

pub fn archive_directory(root: &Path) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.add_directory("ClipHost-Rust/", options).unwrap();
    let mut files: Vec<_> = fs::read_dir(root).unwrap().map(Result::unwrap).collect();
    files.sort_by_key(|entry| entry.file_name());
    for entry in files {
        zip.start_file(
            format!("ClipHost-Rust/{}", entry.file_name().to_string_lossy()),
            options,
        )
        .unwrap();
        zip.write_all(&fs::read(entry.path()).unwrap()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

pub fn serve_once(
    body: Vec<u8>,
    advertised_length: Option<u64>,
    redirect: bool,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = format!(
        "http://{}/eaclip-2.0.0-windows-x64.zip",
        listener.local_addr().unwrap()
    );
    let task = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "loopback fixture received no request"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("loopback fixture accept failed: {error}"),
            }
        };
        // Windows accepted sockets inherit FIONBIO from the listener. Keep
        // accept bounded via polling, but stream large fixtures in blocking
        // mode with the explicit read/write timeouts below.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = [0_u8; 8192];
        stream.read(&mut request).unwrap();
        if redirect {
            let _ = write!(stream, "HTTP/1.1 302 Found\r\nLocation: http://192.0.2.1:9/never-contact\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", advertised_length.unwrap_or(body.len() as u64));
        for chunk in body.chunks(32 * 1024) {
            if stream.write_all(chunk).is_err() {
                break;
            }
        }
    });
    (address, task)
}

pub fn report_directory() -> Option<PathBuf> {
    let directory = PathBuf::from(std::env::var_os("CLIPHOST_UPDATER_SIMULATION_REPORT_DIR")?);
    let allowed =
        fs::canonicalize(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp")).unwrap();
    assert!(
        directory.is_absolute()
            && !directory
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
        "simulation report path must be absolute without traversal"
    );
    let ancestor = directory.ancestors().find(|p| p.exists()).unwrap();
    assert!(
        fs::canonicalize(ancestor).unwrap().starts_with(&allowed),
        "simulation report parent must stay in repository .tmp"
    );
    fs::create_dir_all(&directory).unwrap();
    let canonical = fs::canonicalize(directory).unwrap();
    assert!(
        canonical.starts_with(allowed),
        "simulation reports must stay in repository .tmp"
    );
    Some(canonical)
}
