use crate::package::{self, Manifest, PayloadFile, Version};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub(crate) fn temporary() -> tempfile::TempDir {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/updater-tests");
    fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("case-")
        .tempdir_in(root)
        .unwrap()
}

pub(crate) fn fake_pe(marker: u8) -> Vec<u8> {
    let mut bytes = vec![marker; 128];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[60..64].copy_from_slice(&64_u32.to_le_bytes());
    bytes[64..68].copy_from_slice(b"PE\0\0");
    bytes[68..70].copy_from_slice(&0x8664_u16.to_le_bytes());
    bytes[88..90].copy_from_slice(&0x20b_u16.to_le_bytes());
    bytes
}

pub(crate) fn make_package(root: &Path, version: &str, marker: u8) -> Manifest {
    fs::create_dir_all(root).unwrap();
    let mut files = Vec::new();
    for name in ["cliphost-app.exe", "vcruntime140.dll"] {
        let bytes = fake_pe(marker);
        fs::write(root.join(name), &bytes).unwrap();
        files.push(PayloadFile {
            name: name.into(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            size_bytes: bytes.len() as u64,
        });
    }
    let info = Manifest {
        product: "ClipHost Rust".into(),
        version: version.into(),
        executable: "cliphost-app.exe".into(),
        sha256: files[0].sha256.clone(),
        renderer: "gpui".into(),
        profile: "release".into(),
        architecture: "x64".into(),
        target: Some("x86_64-pc-windows-msvc".into()),
        local_deployment: false,
        files,
    };
    fs::write(
        root.join("build-info.json"),
        serde_json::to_vec_pretty(&info).unwrap(),
    )
    .unwrap();
    fs::write(root.join("README.txt"), b"Fixture release\n").unwrap();
    fs::write(root.join("SHA256SUMS.txt"), b"Fixture sums\n").unwrap();
    info
}

fn archive(root: &Path, destination: &Path, extra: Option<&str>) {
    let mut zip = zip::ZipWriter::new(fs::File::create(destination).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.add_directory("ClipHost-Rust/", options).unwrap();
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        zip.start_file(
            format!("ClipHost-Rust/{}", entry.file_name().to_string_lossy()),
            options,
        )
        .unwrap();
        zip.write_all(&fs::read(entry.path()).unwrap()).unwrap();
    }
    if let Some(name) = extra {
        zip.start_file(name, options).unwrap();
        zip.write_all(b"untrusted").unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn versions_are_numeric_and_reject_prerelease_or_aliases() {
    assert!(Version::parse("0.31.100.0").unwrap() > Version::parse("0.31.99").unwrap());
    assert_eq!(
        Version::parse("0.31.31").unwrap(),
        Version::parse("0.31.31.0").unwrap()
    );
    for bad in [
        "01.2.3",
        "1.2",
        "1.2.3-beta",
        "1.2.3.0.0",
        "1.2.-1",
        "1.2.4294967296",
    ] {
        assert!(Version::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn archive_validates_payload_and_rejects_traversal_unknown_and_case_duplicates() {
    let temp = temporary();
    let root = temp.path().join("source");
    make_package(&root, "1.2.3.0", 1);
    let zip = temp.path().join("package.zip");
    archive(&root, &zip, None);
    assert!(package::extract_archive(&zip, &temp.path().join("valid"), "1.2.3.0").is_ok());
    for (index, path) in [
        "ClipHost-Rust/../outside.exe",
        "ClipHost-Rust/userdata.db",
        "ClipHost-Rust/CLIPHOST-APP.EXE",
        "other/cliphost-app.exe",
        "ClipHost-Rust/file.txt:ads",
    ]
    .iter()
    .enumerate()
    {
        let zip = temp.path().join(format!("bad-{index}.zip"));
        archive(&root, &zip, Some(path));
        assert!(
            package::extract_archive(&zip, &temp.path().join(format!("bad-{index}")), "1.2.3.0")
                .is_err(),
            "{path}"
        );
    }
    assert!(!temp.path().join("outside.exe").exists());
}

#[test]
fn archive_rejects_wrong_version_hash_architecture_and_links() {
    let temp = temporary();
    let root = temp.path().join("source");
    let info = make_package(&root, "1.2.3.0", 2);
    let zip = temp.path().join("package.zip");
    archive(&root, &zip, None);
    assert!(package::extract_archive(&zip, &temp.path().join("version"), "1.2.4.0").is_err());
    fs::write(root.join("cliphost-app.exe"), fake_pe(3)).unwrap();
    archive(&root, &temp.path().join("bad-sha.zip"), None);
    assert!(package::extract_archive(
        &temp.path().join("bad-sha.zip"),
        &temp.path().join("hash"),
        "1.2.3.0"
    )
    .is_err());
    let mut wrong = info;
    wrong.architecture = "arm64".into();
    assert!(package::validate_manifest(&wrong, "1.2.3.0", true).is_err());
    let mut zip = zip::ZipWriter::new(fs::File::create(temp.path().join("link.zip")).unwrap());
    zip.add_symlink(
        "ClipHost-Rust/cliphost-app.exe",
        "../../outside.exe",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    zip.finish().unwrap();
    assert!(package::extract_archive(
        &temp.path().join("link.zip"),
        &temp.path().join("links"),
        "1.2.3.0"
    )
    .is_err());
}

#[test]
#[ignore = "Requires explicit previously downloaded official ZIP in repository .tmp; never downloads during unit tests"]
fn existing_official_archive_fixture() {
    let archive = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.tmp/in-app-update-source-probe/eaclip-0.31.31-windows-x64.zip");
    let temp = temporary();
    let info =
        package::extract_archive(&archive, &temp.path().join("actual"), "0.31.31.0").unwrap();
    assert_eq!(info.files.len(), 13);
    assert!(info.files.iter().any(|f| f.name == "vccorlib140.dll"));
}
