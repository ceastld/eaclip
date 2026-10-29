//! Isolated visual check of the top-edge file line with fixed tags and sheets.

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!("file_line_visual requires a Windows Debug build");
    std::process::exit(2);
}

#[cfg(all(windows, debug_assertions))]
fn main() -> anyhow::Result<()> {
    cliphost_gpui::file_line::fixture::run(std::env::args().skip(1))
}
