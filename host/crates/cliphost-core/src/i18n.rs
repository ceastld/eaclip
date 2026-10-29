//! Chinese and English interface text, using the same paired-text approach as RustShot.
//! Language changes only presentation; configuration keys and user content remain stable.
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LanguageMode {
    #[default]
    System,
    Chinese,
    English,
}

impl LanguageMode {
    pub fn from_wire(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "zh" | "zh-cn" | "chinese" => Self::Chinese,
            "en" | "en-us" | "english" => Self::English,
            _ => Self::System,
        }
    }

    pub fn wire_name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Chinese => "zh-CN",
            Self::English => "en",
        }
    }

    fn resolved(self) -> Self {
        match self {
            Self::System => system_language(),
            mode => mode,
        }
    }
}

static CURRENT_MODE: AtomicU8 = AtomicU8::new(0);
// Compatibility clients keep their existing Chinese schema until a renderer
// explicitly applies its preference. GPUI applies System or the saved mode at startup.
static CURRENT_LANGUAGE: AtomicU8 = AtomicU8::new(1);

fn encode(mode: LanguageMode) -> u8 {
    match mode {
        LanguageMode::System => 0,
        LanguageMode::Chinese => 1,
        LanguageMode::English => 2,
    }
}

fn decode(mode: u8) -> LanguageMode {
    match mode {
        1 => LanguageMode::Chinese,
        2 => LanguageMode::English,
        _ => LanguageMode::System,
    }
}

fn language_for_ui_id(id: u16) -> LanguageMode {
    if id & 0x3ff == 0x04 {
        LanguageMode::Chinese
    } else {
        LanguageMode::English
    }
}

pub fn system_language() -> LanguageMode {
    #[cfg(windows)]
    {
        // The Win32 API is exported by Kernel32 on every supported Windows version.
        #[link(name = "kernel32")]
        extern "system" {
            fn GetUserDefaultUILanguage() -> u16;
        }
        language_for_ui_id(unsafe { GetUserDefaultUILanguage() })
    }
    #[cfg(not(windows))]
    {
        LanguageMode::English
    }
}

/// Apply the configured mode and report whether the displayed language changed.
/// Calling this again for System also detects a Windows display-language change.
pub fn set_language(mode: LanguageMode) -> bool {
    CURRENT_MODE.store(encode(mode), Ordering::Relaxed);
    let resolved = encode(mode.resolved());
    CURRENT_LANGUAGE.swap(resolved, Ordering::Relaxed) != resolved
}

/// The selected preference; System remains System after resolving its display language.
pub fn language() -> LanguageMode {
    decode(CURRENT_MODE.load(Ordering::Relaxed))
}

pub fn displayed_language() -> LanguageMode {
    match decode(CURRENT_LANGUAGE.load(Ordering::Relaxed)) {
        LanguageMode::System => system_language(),
        mode => mode,
    }
}

pub fn is_english() -> bool {
    // Core unit tests assert legacy Chinese contracts and must not depend on this machine.
    if cfg!(test) {
        return false;
    }
    displayed_language() == LanguageMode::English
}

pub fn tr<'a>(chinese: &'a str, english: &'a str) -> &'a str {
    if is_english() {
        english
    } else {
        chinese
    }
}

/// An explicit language variant, independent of global process state and unit-test defaults.
pub fn tr_for<'a>(mode: LanguageMode, chinese: &'a str, english: &'a str) -> &'a str {
    if mode.resolved() == LanguageMode::English {
        english
    } else {
        chinese
    }
}

/// Paired text for constant selection tables; resolve it when producing the interface.
#[derive(Clone, Copy, Debug)]
pub struct Text(pub &'static str, pub &'static str);
impl Text {
    pub fn get(self) -> &'static str {
        tr(self.0, self.1)
    }
}

#[macro_export]
macro_rules! trf {
    ($zh:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::is_english() { format!($en $(, $arg)*) } else { format!($zh $(, $arg)*) }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_wire_round_trip_and_unknown_defaults_to_system() {
        for mode in [
            LanguageMode::System,
            LanguageMode::Chinese,
            LanguageMode::English,
        ] {
            assert_eq!(LanguageMode::from_wire(mode.wire_name()), mode);
        }
        for value in ["", "fr", "broken"] {
            assert_eq!(LanguageMode::from_wire(value), LanguageMode::System);
        }
        assert_eq!(LanguageMode::from_wire(" en-US "), LanguageMode::English);
        assert_eq!(LanguageMode::from_wire("ZH-cn"), LanguageMode::Chinese);
    }

    #[test]
    fn windows_chinese_variants_use_chinese_and_other_languages_fall_back_to_english() {
        for id in [0x0804, 0x0404, 0x0c04, 0x1004, 0x1404] {
            assert_eq!(language_for_ui_id(id), LanguageMode::Chinese);
        }
        for id in [0, 0x0409, 0x0411, 0x040c] {
            assert_eq!(language_for_ui_id(id), LanguageMode::English);
        }
        assert_eq!(
            tr_for(LanguageMode::English, "设置", "Settings"),
            "Settings"
        );
        assert_eq!(tr_for(LanguageMode::Chinese, "设置", "Settings"), "设置");
    }
}
