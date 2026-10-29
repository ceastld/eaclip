//! Isolated visual check of the production cascading menu rows.

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!("super_menu_style requires a Windows Debug build");
    std::process::exit(2);
}

#[cfg(all(windows, debug_assertions))]
fn main() -> anyhow::Result<()> {
    cliphost_gpui::super_menu::style_fixture::run(std::env::args().skip(1))
}
