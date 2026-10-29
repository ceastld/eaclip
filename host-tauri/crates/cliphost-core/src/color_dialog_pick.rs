//! Native settings color selection. The original WPF alpha is never edited by
//! Windows' RGB-only common dialog, and a retired owner cannot accept its reply.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ArgbColor {
    alpha: u8,
    red: u8,
    green: u8,
    blue: u8,
}

fn parse_color(initial: &str) -> Option<ArgbColor> {
    let hex = initial.trim().strip_prefix('#')?;
    if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(ArgbColor {
        alpha: if hex.len() == 8 {
            (value >> 24) as u8
        } else {
            0xff
        },
        red: (value >> 16) as u8,
        green: (value >> 8) as u8,
        blue: value as u8,
    })
}

impl ArgbColor {
    fn color_ref(self) -> u32 {
        u32::from(self.red) | (u32::from(self.green) << 8) | (u32::from(self.blue) << 16)
    }

    fn with_color_ref(self, color_ref: u32) -> String {
        format!(
            "#{:02X}{:02X}{:02X}{:02X}",
            self.alpha,
            color_ref as u8,
            (color_ref >> 8) as u8,
            (color_ref >> 16) as u8,
        )
    }
}

/// The same return guard applies to a color, cancellation and native failure;
/// an old modal reply must never become a new settings page's result.
fn run_guarded_color_picker(
    initial: &str,
    owner: isize,
    verify: &impl Fn() -> Result<(), String>,
    show: impl FnOnce(u32, isize) -> Result<Option<u32>, String>,
) -> Result<Option<String>, String> {
    verify()?;
    // Match the settings color editor's initial RGB fallback. Cancelling
    // still returns None, so an invalid draft is only replaced on acceptance.
    let color = parse_color(initial).unwrap_or(ArgbColor {
        alpha: 0xff,
        red: 0xff,
        green: 0xff,
        blue: 0xff,
    });
    verify()?;
    let result = show(color.color_ref(), owner);
    verify()?;
    result.map(|selected| selected.map(|rgb| color.with_color_ref(rgb)))
}

pub(crate) fn pick_color_guarded(
    initial: &str,
    owner: isize,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<Option<String>, String> {
    run_guarded_color_picker(initial, owner, verify, show_color)
}

#[cfg(all(windows, not(test)))]
fn show_color(initial_rgb: u32, owner: isize) -> Result<Option<u32>, String> {
    use windows::Win32::{
        Foundation::{COLORREF, HWND},
        UI::Controls::Dialogs::{
            ChooseColorW, CommDlgExtendedError, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW,
        },
    };
    if !crate::clipboard::native_dialogs_enabled() {
        return Ok(None);
    }
    // Keep the structure and custom-color storage on the calling worker's
    // stack throughout the synchronous modal. No host or UI borrow is held.
    let mut custom_colors = [COLORREF(0); 16];
    let mut dialog = CHOOSECOLORW {
        lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: HWND(owner as *mut _),
        rgbResult: COLORREF(initial_rgb),
        lpCustColors: custom_colors.as_mut_ptr(),
        Flags: CC_RGBINIT | CC_FULLOPEN,
        ..Default::default()
    };
    unsafe {
        if ChooseColorW(&mut dialog).as_bool() {
            Ok(Some(dialog.rgbResult.0))
        } else {
            // Read on this same thread immediately after false, before a
            // lease predicate could make another native call. Zero is cancel.
            let error = CommDlgExtendedError().0;
            if error == 0 {
                Ok(None)
            } else {
                Err(format!("原生颜色选择器失败（错误码 {error:#x}）。"))
            }
        }
    }
}

// Pure tests cannot fall through to a physical dialog, even with dialogs
// enabled in the environment. Other platforms have no native color dialog.
#[cfg(any(test, not(windows)))]
fn show_color(_: u32, _: isize) -> Result<Option<u32>, String> {
    Ok(None)
}

#[cfg(test)]
#[path = "color_dialog_pick_tests.rs"]
mod tests;
