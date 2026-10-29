use gpui::{AssetSource, SharedString};
use std::borrow::Cow;

include!(concat!(env!("OUT_DIR"), "/design-assets.rs"));

/// Design assets are compiled into the native binary. No Vite, web files or
/// working-directory assumptions are needed when the preview is launched.
pub struct DesignAssets;

impl AssetSource for DesignAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(embedded_asset(path).map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(ASSET_NAMES
            .iter()
            .filter(|name| name.starts_with(path))
            .map(|name| SharedString::from(*name))
            .collect())
    }
}
