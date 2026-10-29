#[cfg(all(windows, debug_assertions))]
fn main() -> anyhow::Result<()> {
    cliphost_gpui::super_menu::browser_workspace::run_visual_fixture()
}

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    panic!("This fixture requires a Windows Debug build");
}
