//! Display-only recognition of the existing synthetic history sources.
//! These are receive-channel hints, not trusted device identity or hardware data.

pub(crate) fn icon_name(source: &str) -> Option<&'static str> {
    let source = source.trim();
    if source.eq_ignore_ascii_case("ClipHost Device Sync") {
        Some("desktop")
    } else if source.eq_ignore_ascii_case("ClipHost Sync") {
        Some("phone")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_sync_sources_are_recognized_without_rewriting_records() {
        assert_eq!(icon_name("ClipHost Device Sync"), Some("desktop"));
        assert_eq!(icon_name("  cliphost device SYNC\t"), Some("desktop"));
        assert_eq!(icon_name("ClipHost Sync"), Some("phone"));
    }

    #[test]
    fn ordinary_programs_are_not_mistaken_for_sync_sources() {
        for source in ["", "Sync.exe", "ClipHost Sync.exe", "ClipHost Device Sync.exe",
            "My ClipHost Sync", "ClipHost Device Sync Helper", "notepad.exe"] {
            assert_eq!(icon_name(source), None, "{source}");
        }
    }
}
