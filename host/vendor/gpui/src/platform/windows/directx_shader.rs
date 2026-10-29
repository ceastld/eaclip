use anyhow::Result;

#[cfg(debug_assertions)]
use windows::{
    Win32::Graphics::Direct3D::{
        Fxc::{D3DCOMPILE_DEBUG, D3DCOMPILE_SKIP_OPTIMIZATION, D3DCompile},
        ID3DBlob, ID3DInclude,
    },
    core::{PCSTR, s},
};

// A deployed Debug executable can outlive the checkout that built it. Embed
// both programs and their shared include so creating another window never
// needs that checkout, the process working directory, or an include handler.
#[cfg(debug_assertions)]
fn embedded_source(module: ShaderModule) -> (&'static str, PCSTR) {
    use std::sync::LazyLock;

    const INCLUDE: &str = "#include \"alpha_correction.hlsl\"";
    const ALPHA: &str = include_str!("alpha_correction.hlsl");
    static SHADERS: LazyLock<String> =
        LazyLock::new(|| include_str!("shaders.hlsl").replacen(INCLUDE, ALPHA, 1));
    static EMOJI: LazyLock<String> =
        LazyLock::new(|| include_str!("color_text_raster.hlsl").replacen(INCLUDE, ALPHA, 1));

    if matches!(module, ShaderModule::EmojiRasterization) {
        (&EMOJI, s!("gpui-embedded://color_text_raster.hlsl"))
    } else {
        (&SHADERS, s!("gpui-embedded://shaders.hlsl"))
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum ShaderModule {
    Quad,
    Shadow,
    Underline,
    PathRasterization,
    PathSprite,
    MonochromeSprite,
    PolychromeSprite,
    EmojiRasterization,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum ShaderTarget {
    Vertex,
    Fragment,
}

pub(crate) struct RawShaderBytes<'t> {
    inner: &'t [u8],

    #[cfg(debug_assertions)]
    _blob: ID3DBlob,
}

impl<'t> RawShaderBytes<'t> {
    pub(crate) fn new(module: ShaderModule, target: ShaderTarget) -> Result<Self> {
        #[cfg(not(debug_assertions))]
        {
            Ok(Self::from_bytes(module, target))
        }
        #[cfg(debug_assertions)]
        {
            let blob = build_shader_blob(module, target)?;
            let inner = unsafe {
                std::slice::from_raw_parts(
                    blob.GetBufferPointer() as *const u8,
                    blob.GetBufferSize(),
                )
            };
            Ok(Self { inner, _blob: blob })
        }
    }

    pub(crate) fn as_bytes(&'t self) -> &'t [u8] {
        self.inner
    }

    #[cfg(not(debug_assertions))]
    fn from_bytes(module: ShaderModule, target: ShaderTarget) -> Self {
        let bytes = match module {
            ShaderModule::Quad => match target {
                ShaderTarget::Vertex => QUAD_VERTEX_BYTES,
                ShaderTarget::Fragment => QUAD_FRAGMENT_BYTES,
            },
            ShaderModule::Shadow => match target {
                ShaderTarget::Vertex => SHADOW_VERTEX_BYTES,
                ShaderTarget::Fragment => SHADOW_FRAGMENT_BYTES,
            },
            ShaderModule::Underline => match target {
                ShaderTarget::Vertex => UNDERLINE_VERTEX_BYTES,
                ShaderTarget::Fragment => UNDERLINE_FRAGMENT_BYTES,
            },
            ShaderModule::PathRasterization => match target {
                ShaderTarget::Vertex => PATH_RASTERIZATION_VERTEX_BYTES,
                ShaderTarget::Fragment => PATH_RASTERIZATION_FRAGMENT_BYTES,
            },
            ShaderModule::PathSprite => match target {
                ShaderTarget::Vertex => PATH_SPRITE_VERTEX_BYTES,
                ShaderTarget::Fragment => PATH_SPRITE_FRAGMENT_BYTES,
            },
            ShaderModule::MonochromeSprite => match target {
                ShaderTarget::Vertex => MONOCHROME_SPRITE_VERTEX_BYTES,
                ShaderTarget::Fragment => MONOCHROME_SPRITE_FRAGMENT_BYTES,
            },
            ShaderModule::PolychromeSprite => match target {
                ShaderTarget::Vertex => POLYCHROME_SPRITE_VERTEX_BYTES,
                ShaderTarget::Fragment => POLYCHROME_SPRITE_FRAGMENT_BYTES,
            },
            ShaderModule::EmojiRasterization => match target {
                ShaderTarget::Vertex => EMOJI_RASTERIZATION_VERTEX_BYTES,
                ShaderTarget::Fragment => EMOJI_RASTERIZATION_FRAGMENT_BYTES,
            },
        };
        Self { inner: bytes }
    }
}

#[cfg(debug_assertions)]
pub(super) fn build_shader_blob(entry: ShaderModule, target: ShaderTarget) -> Result<ID3DBlob> {
    unsafe {
        let (source, source_name) = embedded_source(entry);

        let entry = format!(
            "{}_{}\0",
            entry.as_str(),
            match target {
                ShaderTarget::Vertex => "vertex",
                ShaderTarget::Fragment => "fragment",
            }
        );
        let target = match target {
            ShaderTarget::Vertex => "vs_4_1\0",
            ShaderTarget::Fragment => "ps_4_1\0",
        };

        let mut compile_blob = None;
        let mut error_blob = None;

        let entry_point = PCSTR::from_raw(entry.as_ptr());
        let target_cstr = PCSTR::from_raw(target.as_ptr());

        let ret = D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            source_name,
            None,
            None::<&ID3DInclude>,
            entry_point,
            target_cstr,
            D3DCOMPILE_DEBUG | D3DCOMPILE_SKIP_OPTIMIZATION,
            0,
            &mut compile_blob,
            Some(&mut error_blob),
        );
        if let Err(error) = ret {
            let Some(error_blob) = error_blob else {
                return Err(anyhow::anyhow!(
                    "Compiling embedded shader {} failed: {error}",
                    entry.trim_end_matches('\0')
                ));
            };

            let error_bytes = std::slice::from_raw_parts(
                error_blob.GetBufferPointer().cast::<u8>(),
                error_blob.GetBufferSize(),
            );
            let error_string = String::from_utf8_lossy(error_bytes);
            let error_string = error_string.trim_end_matches('\0');
            log::error!("Shader compile error: {}", error_string);
            return Err(anyhow::anyhow!(
                "Compiling embedded shader {} failed: {error}: {error_string}",
                entry.trim_end_matches('\0')
            ));
        }
        compile_blob.ok_or_else(|| anyhow::anyhow!("D3DCompile returned no shader bytecode"))
    }
}

#[cfg(not(debug_assertions))]
include!(concat!(env!("OUT_DIR"), "/shaders_bytes.rs"));

#[cfg(debug_assertions)]
impl ShaderModule {
    pub fn as_str(&self) -> &str {
        match self {
            ShaderModule::Quad => "quad",
            ShaderModule::Shadow => "shadow",
            ShaderModule::Underline => "underline",
            ShaderModule::PathRasterization => "path_rasterization",
            ShaderModule::PathSprite => "path_sprite",
            ShaderModule::MonochromeSprite => "monochrome_sprite",
            ShaderModule::PolychromeSprite => "polychrome_sprite",
            ShaderModule::EmojiRasterization => "emoji_rasterization",
        }
    }
}
