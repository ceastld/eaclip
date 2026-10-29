//! Isolated Debug check of real menu-row hover events and preview ownership.

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!("super_menu_file_preview_visual requires a Windows Debug build");
    std::process::exit(2);
}

#[cfg(all(windows, debug_assertions))]
fn main() -> anyhow::Result<()> {
    cliphost_gpui::super_menu::file_preview_fixture::run(std::env::args().skip(1))
}
