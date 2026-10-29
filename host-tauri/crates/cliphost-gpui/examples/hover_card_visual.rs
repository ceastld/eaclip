//! Isolated visual check of the GPUI hover card (rich card, file list, previews).

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!("hover_card_visual requires a Windows Debug build");
    std::process::exit(2);
}

#[cfg(all(windows, debug_assertions))]
fn main() -> anyhow::Result<()> {
    cliphost_gpui::hover_card::fixture::run(std::env::args().skip(1))
}
