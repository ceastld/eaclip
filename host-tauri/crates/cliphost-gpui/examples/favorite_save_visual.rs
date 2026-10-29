//! Offline, no-activate production folder chooser. No global input or capture.
#[cfg(not(all(windows, debug_assertions)))]
fn main() { eprintln!("favorite_save_visual requires a Windows Debug build"); std::process::exit(2); }

#[cfg(all(windows, debug_assertions))]
fn main() -> anyhow::Result<()> {
    cliphost_gpui::favorite_save::fixture::run(std::env::args().skip(1))
}
