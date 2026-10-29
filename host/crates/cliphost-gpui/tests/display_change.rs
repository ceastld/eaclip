// Test the exact vendored HWND helper without constructing a GPUI renderer.
// Vendored dependencies cannot run their own dev-dependency tests as packages
// outside this workspace, so this integration target includes the same module.
#[cfg(windows)]
#[path = "../../../vendor/gpui/src/platform/windows/display_change.rs"]
mod display_change;
