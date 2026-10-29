//! Isolated, offline rendering of the production LAN management page.
//! Build in Debug; run through scripts/Test-GpuiLanPreview.ps1.

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!("lan_visual requires a Windows Debug build");
    std::process::exit(2);
}

#[cfg(all(windows, debug_assertions))]
fn main() -> anyhow::Result<()> {
    cliphost_gpui::management::lan_fixture::run(std::env::args().skip(1))
}
