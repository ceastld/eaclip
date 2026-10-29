//! Disposable native-menu preview. No host, tray icon, clipboard or data root.
//! Reuses the production theme resolver and native API adapter verbatim.

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!("tray_theme_visual requires a Windows Debug build");
    std::process::exit(2);
}

#[cfg(all(windows, debug_assertions))]
use cliphost_gpui::preview;
#[cfg(all(windows, debug_assertions))]
#[allow(dead_code)]
#[path = "../src/custom_theme.rs"]
mod custom_theme;
#[cfg(all(windows, debug_assertions))]
#[path = "../src/tray_theme.rs"]
mod tray_theme;

#[cfg(all(windows, debug_assertions))]
fn main() -> anyhow::Result<()> {
    use std::{fs, path::PathBuf};
    use windows::{
        core::{w, PCWSTR},
        Win32::{
            Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::*,
        },
    };

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wp: WPARAM,
        lp: LPARAM,
    ) -> LRESULT {
        if message == WM_TIMER {
            let _ = KillTimer(Some(hwnd), 1);
            let _ = EndMenu();
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, message, wp, lp)
    }

    let mut args = std::env::args().skip(1);
    let output = PathBuf::from(
        args.next()
            .ok_or_else(|| anyhow::anyhow!("output directory required"))?,
    );
    anyhow::ensure!(output.is_absolute(), "output directory must be absolute");
    fs::create_dir_all(&output)?;
    let modes: Vec<_> = args.collect();
    anyhow::ensure!(
        !modes.is_empty(),
        "supply dark, light, auto, custom-dark or custom-light"
    );
    unsafe {
        let instance = HINSTANCE(GetModuleHandleW(None)?.0);
        let class_name = w!("ClipHostGpui.TrayThemePreview");
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name,
            ..Default::default()
        };
        anyhow::ensure!(
            RegisterClassW(&class) != 0,
            "preview class registration failed"
        );
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name,
            w!("ClipHost tray theme preview"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )?;
        let result = (|| -> anyhow::Result<()> {
            let initial_foreground = GetForegroundWindow();
            for (index, mode) in modes.iter().enumerate() {
                let (setting, background) = match mode.as_str() {
                    "dark" | "light" | "auto" => (mode.as_str(), "#FFFFFFFF"),
                    "custom-dark" => ("custom", "#FF121212"),
                    "custom-light" => ("custom", "#FFF0F0F0"),
                    _ => anyhow::bail!("unknown theme: {mode}"),
                };
                let form = serde_json::json!({"fields":[
                    {"key":"UiThemeMode", "value":setting},
                    {"key":"WindowBackgroundColor", "value":background},
                ]});
                let resolved = tray_theme::ThemeMode::from_global_form(&form);
                let applied = tray_theme::apply(hwnd, resolved);
                anyhow::ensure!(applied, "native menu theme API unavailable");
                let menu = CreatePopupMenu()?;
                let popup = (|| -> anyhow::Result<_> {
                    for (id, label) in [
                        (1, "打开主窗口"),
                        (2, "剪贴板"),
                        (3, "收藏"),
                        (4, "设置"),
                        (5, "帮助"),
                    ] {
                        let label: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
                        AppendMenuW(menu, MF_STRING, id, PCWSTR(label.as_ptr()))?;
                    }
                    AppendMenuW(menu, MF_SEPARATOR, 0, None)?;
                    AppendMenuW(menu, MF_STRING, 6, w!("退出"))?;
                    let report = serde_json::json!({
                        "pid":std::process::id(), "ownerHwnd":hwnd.0 as usize,
                        "index":index, "mode":mode, "resolved":format!("{resolved:?}"),
                        "applied":applied, "foregroundBefore":GetForegroundWindow().0 as usize,
                    });
                    fs::write(
                        output.join("current.json"),
                        serde_json::to_vec_pretty(&report)?,
                    )?;
                    anyhow::ensure!(SetTimer(Some(hwnd), 1, 5_000, None) != 0, "timer failed");
                    // Never activate the preview owner or inject input. The
                    // timer closes the menu even without foreground ownership.
                    let action = TrackPopupMenu(
                        menu,
                        TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                        GetSystemMetrics(SM_CXSCREEN) - 260,
                        GetSystemMetrics(SM_CYSCREEN) - 360,
                        None,
                        hwnd,
                        None,
                    );
                    let _ = KillTimer(Some(hwnd), 1);
                    Ok(serde_json::json!({"mode":mode,"action":action.0,
                        "foregroundAfter":GetForegroundWindow().0 as usize,
                        "foregroundPreserved":GetForegroundWindow()==initial_foreground,
                        "applied":applied, "resolved":format!("{resolved:?}")}))
                })();
                let _ = DestroyMenu(menu);
                let report = popup?;
                fs::write(
                    output.join(format!("{index}-{mode}.json")),
                    serde_json::to_vec_pretty(&report)?,
                )?;
            }
            Ok(())
        })();
        let _ = DestroyWindow(hwnd);
        let _ = UnregisterClassW(class_name, Some(instance));
        result?;
    }
    Ok(())
}
