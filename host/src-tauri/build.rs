fn main() {
    let mut attributes = tauri_build::Attributes::new();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // Cargo JSON points at a mutable target path. Keep the actual build role
        // in the PE so packaging cannot mistake a later fixture for this build.
        let info = format!(
            "format=cliphost-build-v1\nnative_acceptance={}\ncustom_protocol={}\nopt_level={}\nversion={}\ntarget={}\n",
            std::env::var_os("CARGO_FEATURE_NATIVE_ACCEPTANCE").is_some(),
            std::env::var_os("CARGO_FEATURE_CUSTOM_PROTOCOL").is_some(),
            std::env::var("OPT_LEVEL").expect("Cargo optimization level"),
            std::env::var("CARGO_PKG_VERSION").expect("Cargo package version"),
            std::env::var("TARGET").expect("Cargo target"),
        );
        let path = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"))
            .join("cliphost-build-info.txt");
        std::fs::write(&path, info).expect("write executable build metadata");
        let resource_path = path.to_string_lossy().replace('\\', "/").replace('"', "\\\"");
        attributes = attributes.windows_attributes(
            tauri_build::WindowsAttributes::new()
                .append_rc_content(format!("CLIPHOST_BUILD_INFO RCDATA \"{resource_path}\"")),
        );
    }
    tauri_build::try_build(attributes).expect("build Tauri resources");
}
