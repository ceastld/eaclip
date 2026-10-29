//! Isolated visual check of the click-through drag preview popup.

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!("drag_preview_visual requires a Windows Debug build");
    std::process::exit(2);
}

#[cfg(all(windows, debug_assertions))]
fn main() -> anyhow::Result<()> {
    cliphost_gpui::drag_preview::fixture::run(std::env::args().skip(1))
}
