#![cfg(all(target_os = "windows", debug_assertions))]

// Compile the exact production shader module without creating a DirectX
// device or a window. The child runs with no source files in its directory.
#[path = "../../../vendor/gpui/src/platform/windows/directx_shader.rs"]
mod shader_resources;

#[test]
fn embedded_shaders_compile_without_source_tree() {
    const CHILD: &str = "CLIPHOST_EMBEDDED_SHADER_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let test_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&test_root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("directx-embedded-shaders-")
            .tempdir_in(test_root)
            .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "embedded_shaders_compile_without_source_tree",
                "--nocapture",
                "--test-threads=1",
            ])
            .current_dir(directory.path())
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "embedded shader child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    assert_eq!(std::fs::read_dir(".").unwrap().count(), 0);
    // This integration crate's baked manifest root has no shader sources.
    // Restoring the old env!("CARGO_MANIFEST_DIR") file loader would fail here,
    // even though the vendor checkout still exists elsewhere on the machine.
    let old_shader_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/platform/windows/shaders.hlsl");
    assert!(!old_shader_path.exists());

    use shader_resources::{RawShaderBytes, ShaderModule, ShaderTarget};
    for module in [
        ShaderModule::Quad,
        ShaderModule::Shadow,
        ShaderModule::Underline,
        ShaderModule::PathRasterization,
        ShaderModule::PathSprite,
        ShaderModule::MonochromeSprite,
        ShaderModule::PolychromeSprite,
        ShaderModule::EmojiRasterization,
    ] {
        for stage in [ShaderTarget::Vertex, ShaderTarget::Fragment] {
            let bytecode = RawShaderBytes::new(module, stage)
                .unwrap_or_else(|error| panic!("{module:?}/{stage:?}: {error:#}"));
            let bytes = bytecode.as_bytes();
            assert!(bytes.len() > 4, "{module:?}/{stage:?} has no bytecode");
            assert_eq!(&bytes[..4], b"DXBC", "{module:?}/{stage:?}");
        }
    }
}
