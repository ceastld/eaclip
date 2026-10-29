//! Fold overrides only the native minimum height; all other GPUI limits stay.

/// Convert the folded logical client height using the HWND's current DPI and
/// measured frame inset, rather than GPUI's possibly delayed DPI/frame cache.
pub fn minimum_outer_height(
    client_height_dip: f64,
    dpi: u32,
    outer_height: i64,
    client_height: i64,
) -> Option<i32> {
    if !client_height_dip.is_finite()
        || client_height_dip <= 0.0
        || dpi == 0
        || client_height < 0
        || outer_height < client_height
    {
        return None;
    }
    let physical = (client_height_dip * f64::from(dpi) / 96.0).round();
    if !physical.is_finite() || physical < 1.0 || physical > f64::from(i32::MAX) {
        return None;
    }
    let inset = outer_height.checked_sub(client_height)?;
    i32::try_from((physical as i64).checked_add(inset)?).ok()
}

#[cfg(test)]
#[path = "fold_geometry_tests.rs"]
mod tests;
