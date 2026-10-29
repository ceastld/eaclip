//! Resolve the retained tray's native menu appearance from current global
//! preferences. Win32 menus support light/dark palettes, so Custom uses the
//! same background brightness boundary as the existing GPUI palette.

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThemeMode {
    System,
    Dark,
    Light,
}

impl ThemeMode {
    pub(crate) fn from_global_form(form: &Value) -> Self {
        let value = |key| {
            form["fields"]
                .as_array()?
                .iter()
                .find(|field| field["key"].as_str() == Some(key))?["value"]
                .as_str()
        };
        let mode = value("UiThemeMode").unwrap_or("auto");
        // Custom follows the palette's existing JS trim, while the root's
        // explicit light/dark selection does not trim its mode.
        if crate::custom_theme::is_custom_mode(mode) {
            let background = value("WindowBackgroundColor");
            return if crate::custom_theme::persisted_background_is_dark(background) {
                Self::Dark
            } else {
                Self::Light
            };
        }
        match mode.to_ascii_lowercase().as_str() {
            "dark" => Self::Dark,
            "light" => Self::Light,
            _ => Self::System,
        }
    }

    #[cfg(windows)]
    fn preferred_app_mode(self) -> i32 {
        match self {
            Self::System => 1,
            Self::Dark => 2,
            Self::Light => 3,
        }
    }
}

// Ordinal 135 had a different ABI before Windows 10 1903. Finding that export
// alone is insufficient to call it as SetPreferredAppMode.
#[cfg(any(windows, test))]
fn supports_preferred_app_mode(major: u32, build: u32) -> bool {
    major >= 10 && build >= 18_362
}

#[cfg(windows)]
mod win {
    use std::sync::OnceLock;

    use windows::{
        core::{s, w, PCSTR},
        Win32::{
            Foundation::{FreeLibrary, HWND},
            System::LibraryLoader::{
                GetModuleHandleW, GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
            },
        },
    };

    use super::{supports_preferred_app_mode, ThemeMode};

    type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
    // These private uxtheme exports use C++ bool rather than Win32 BOOL.
    type AllowDarkModeForWindow = unsafe extern "system" fn(HWND, bool) -> bool;
    type FlushMenuThemes = unsafe extern "system" fn();
    type RefreshImmersiveColorPolicyState = unsafe extern "system" fn();

    struct MenuThemeApi {
        set_mode: SetPreferredAppMode,
        allow_window: AllowDarkModeForWindow,
        flush: FlushMenuThemes,
        refresh_policy: Option<RefreshImmersiveColorPolicyState>,
    }

    #[repr(C)]
    struct OsVersionInfo {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }

    unsafe fn supported_windows_version() -> bool {
        type RtlGetVersion = unsafe extern "system" fn(*mut OsVersionInfo) -> i32;
        let Ok(ntdll) = GetModuleHandleW(w!("ntdll.dll")) else {
            return false;
        };
        let Some(get_version) = GetProcAddress(ntdll, s!("RtlGetVersion")) else {
            return false;
        };
        let get_version: RtlGetVersion = std::mem::transmute(get_version);
        let mut version = OsVersionInfo {
            size: std::mem::size_of::<OsVersionInfo>() as u32,
            major: 0,
            minor: 0,
            build: 0,
            platform: 0,
            service_pack: [0; 128],
        };
        get_version(&mut version) >= 0 && supports_preferred_app_mode(version.major, version.build)
    }

    impl MenuThemeApi {
        unsafe fn load() -> Option<Self> {
            if !supported_windows_version() {
                return None;
            }
            let library =
                LoadLibraryExW(w!("uxtheme.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
            let ordinal = |number: usize| GetProcAddress(library, PCSTR(number as *const u8));
            let (Some(set_mode), Some(allow_window), Some(flush)) =
                (ordinal(135), ordinal(133), ordinal(136))
            else {
                let _ = FreeLibrary(library);
                return None;
            };
            let refresh_policy = ordinal(104).map(|function| {
                std::mem::transmute::<_, RefreshImmersiveColorPolicyState>(function)
            });
            // The single successful module reference is retained for the
            // process lifetime so cached function pointers cannot be unloaded.
            Some(Self {
                set_mode: std::mem::transmute::<_, SetPreferredAppMode>(set_mode),
                allow_window: std::mem::transmute::<_, AllowDarkModeForWindow>(allow_window),
                flush: std::mem::transmute::<_, FlushMenuThemes>(flush),
                refresh_policy,
            })
        }
    }

    /// Returns whether the supported native theme API was applied. An absent
    /// API leaves the system menu unchanged. Call on the menu owner's thread
    /// before every popup so both settings and System mode remain current.
    pub(crate) unsafe fn apply(hwnd: HWND, mode: ThemeMode) -> bool {
        static API: OnceLock<Option<MenuThemeApi>> = OnceLock::new();
        let Some(api) = API.get_or_init(|| MenuThemeApi::load()) else {
            return false;
        };
        (api.set_mode)(mode.preferred_app_mode());
        if mode == ThemeMode::System {
            if let Some(refresh) = api.refresh_policy {
                refresh();
            }
        }
        let _ = (api.allow_window)(hwnd, mode != ThemeMode::Light);
        (api.flush)();
        true
    }
}

#[cfg(windows)]
pub(crate) use win::apply;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn form(mode: &str, background: Option<&str>) -> Value {
        let mut fields = vec![json!({"key":"UiThemeMode", "value":mode})];
        if let Some(background) = background {
            fields.push(json!({"key":"WindowBackgroundColor", "value":background}));
        }
        json!({"fields":fields})
    }

    #[test]
    fn explicit_modes_match_root_case_and_whitespace_semantics() {
        for (mode, expected) in [
            ("dark", ThemeMode::Dark),
            ("DARK", ThemeMode::Dark),
            ("Light", ThemeMode::Light),
            ("auto", ThemeMode::System),
            ("unknown", ThemeMode::System),
            (" dark ", ThemeMode::System),
        ] {
            assert_eq!(
                ThemeMode::from_global_form(&form(mode, None)),
                expected,
                "{mode}"
            );
        }
        assert_eq!(ThemeMode::from_global_form(&Value::Null), ThemeMode::System);
        assert_eq!(
            ThemeMode::from_global_form(&json!({"fields":[{"key":"UiThemeMode","value":42}]})),
            ThemeMode::System
        );
    }

    #[test]
    fn custom_native_palette_reuses_wpf_background_parser() {
        for (background, expected) in [
            (Some("#FF121212"), ThemeMode::Dark),
            (Some("#FFF0F0F0"), ThemeMode::Light),
            (Some("#B2B2B2"), ThemeMode::Dark),
            (Some("#B3B3B3"), ThemeMode::Light),
            (Some("#00121212"), ThemeMode::Dark),
            (Some("#00FFFFFF"), ThemeMode::Light),
            (Some("#121212"), ThemeMode::Dark),
            (
                Some("\u{FEFF}\u{A0}#80112233\u{A0}\u{FEFF}"),
                ThemeMode::Dark,
            ),
            (Some("#FZ112233"), ThemeMode::Dark),
            (Some("#GG112233"), ThemeMode::Light),
            (Some("invalid"), ThemeMode::Light),
            (None, ThemeMode::Light),
        ] {
            assert_eq!(
                ThemeMode::from_global_form(&form("CUSTOM", background)),
                expected,
                "{background:?}"
            );
        }
    }

    #[test]
    fn custom_mode_matches_existing_js_whitespace_semantics() {
        for (mode, expected) in [
            (" custom ", ThemeMode::Dark),
            ("\u{FEFF}\u{A0}CuStOm\u{A0}\u{FEFF}", ThemeMode::Dark),
            ("\u{85}custom\u{85}", ThemeMode::System),
        ] {
            assert_eq!(
                ThemeMode::from_global_form(&form(mode, Some("#FF121212"))),
                expected,
                "{mode:?}"
            );
        }
    }

    #[test]
    fn preferred_app_mode_requires_windows_10_1903_or_newer() {
        assert!(!supports_preferred_app_mode(6, 99_999));
        assert!(!supports_preferred_app_mode(10, 17_763));
        assert!(!supports_preferred_app_mode(10, 18_361));
        assert!(supports_preferred_app_mode(10, 18_362));
        assert!(supports_preferred_app_mode(10, 22_000));
    }
}
