//! Offline, no-activate production history menu with isolated fixed data.
#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!("history_context_menu_visual requires a Windows Debug build");
    std::process::exit(2);
}

#[cfg(all(windows, debug_assertions))]
fn main() -> anyhow::Result<()> {
    cliphost_gpui::menu::history_fixture::run(std::env::args().skip(1))
}
