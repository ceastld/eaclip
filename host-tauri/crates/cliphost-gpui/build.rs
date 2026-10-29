use std::{env, fs, path::PathBuf};

fn main() {
    embed_build_info();
    let assets =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../../../ui-design/assets");
    println!("cargo:rerun-if-changed={}", assets.display());
    let mut entries: Vec<_> = fs::read_dir(&assets)
        .expect("shared design assets")
        .map(|entry| entry.expect("design asset entry").path())
        .filter(|path| path.is_file())
        .collect();
    entries.sort();
    let mut source =
        String::from("fn embedded_asset(path: &str) -> Option<&'static [u8]> { match path {\n");
    let mut names = Vec::new();
    for path in entries {
        // A directory watch only sees added or removed files; track contents too.
        println!("cargo:rerun-if-changed={}", path.display());
        let name = path
            .file_name()
            .unwrap()
            .to_str()
            .expect("UTF-8 asset name");
        names.push(format!("{name:?}"));
        source.push_str(&format!(
            "{name:?} => Some(include_bytes!({:?})),\n",
            path.canonicalize().unwrap()
        ));
    }
    source.push_str("_ => None } }\n");
    source.push_str(&format!(
        "const ASSET_NAMES: &[&str] = &[{}];\n",
        names.join(",")
    ));
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("design-assets.rs"),
        source,
    )
    .expect("embed shared design assets");
}

fn embed_build_info() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // Packaging reads this resource without running the host. Keep the real
    // Cargo version, optimization and fixture role bound to the linked EXE.
    let info = format!(
        "format=cliphost-build-v1\nrenderer=gpui\nembedded_frontend=false\nnative_acceptance={}\ncustom_protocol=false\nopt_level={}\nversion={}\ntarget={}\n",
        env::var_os("CARGO_FEATURE_NATIVE_ACCEPTANCE").is_some(),
        env::var("OPT_LEVEL").expect("Cargo optimization level"),
        env::var("CARGO_PKG_VERSION").expect("Cargo package version"),
        env::var("TARGET").expect("Cargo target"),
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    let icon = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
        .join("../../src-tauri/icons/icon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let metadata = out.join("cliphost-build-info.txt");
    fs::write(&metadata, info).expect("write executable build metadata");
    let resource_path = metadata.to_string_lossy().replace('\\', "/").replace('"', "\\\"");
    let icon_path = icon.canonicalize().expect("ClipHost product icon")
        .to_string_lossy().replace('\\', "/").replace('"', "\\\"");
    let resource = out.join("cliphost-build-info.rc");
    // GPUI's Windows backend loads icon group 1 for its window class. The same
    // resource supplies Explorer/shortcuts and the native tray, including all
    // sizes already present in the established ClipHost product ICO.
    let version_info = windows_version_resource();
    fs::write(&resource, format!("1 ICON \"{icon_path}\"\nCLIPHOST_BUILD_INFO RCDATA \"{resource_path}\"\n{version_info}"))
        .expect("write executable metadata resource");
    embed_resource::compile_for(&resource, ["cliphost-gpui"], embed_resource::NONE)
        .manifest_required()
        .expect("embed executable build metadata");
}

fn windows_version_resource() -> String {
    // Windows stores each numeric version component in a WORD. Derive both
    // representations from Cargo and reject versions the PE format cannot hold.
    let [major, minor, patch] = [
        "CARGO_PKG_VERSION_MAJOR",
        "CARGO_PKG_VERSION_MINOR",
        "CARGO_PKG_VERSION_PATCH",
    ]
    .map(|key| {
        env::var(key)
            .expect("Cargo version component")
            .parse::<u16>()
            .expect("Cargo version components must fit Windows VERSIONINFO")
    });
    // VS_FF_DEBUG describes a Debug build, independently of whether a Release
    // profile retains debug symbols. Other VERSIONINFO constants use SDK values.
    let file_flags = if env::var("PROFILE").as_deref() == Ok("debug") { 1 } else { 0 };
    format!(r#"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3fL
FILEFLAGS {file_flags}L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "FileDescription", "ClipHost\0"
            VALUE "FileVersion", "{major}.{minor}.{patch}.0\0"
            VALUE "InternalName", "cliphost-gpui\0"
            VALUE "OriginalFilename", "cliphost-app.exe\0"
            VALUE "ProductName", "ClipHost\0"
            VALUE "ProductVersion", "{major}.{minor}.{patch}.0\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#)
}
