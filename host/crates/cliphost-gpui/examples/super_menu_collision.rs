//! In-process GPUI hover regression, using an isolated core and fixed menu rows.

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!("super_menu_collision requires a Windows Debug build");
    std::process::exit(2);
}

#[cfg(all(windows, debug_assertions))]
fn main() -> anyhow::Result<()> {
    cliphost_gpui::super_menu::collision_fixture::run(std::env::args().skip(1))
}
