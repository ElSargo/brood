//! Bounded text helpers for compact utility surfaces.

/// Truncate by scalar count and append one ellipsis when more input remains.
#[must_use]
pub fn truncate(text: &str, max_chars: usize) -> String {
    let mut characters = text.chars();
    let prefix = characters.by_ref().take(max_chars).collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

/// Cheaply bound labels before asking the host text shaper to clip them.
///
/// This estimate is intended for dense UI labels, not terminal cells or exact
/// measurement. The host layout's `max_width` remains the authoritative clip.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "validated finite positive UI geometry is intentionally reduced to a character budget"
)]
pub fn truncate_to_width(text: &str, max_width: f32, font_size: f32) -> String {
    if !max_width.is_finite() || !font_size.is_finite() || font_size <= 0.0 || max_width < font_size
    {
        return String::new();
    }
    let max_chars = (max_width / (font_size * 0.64)).floor().max(2.0) as usize;
    truncate(text, max_chars)
}

/// Replace line boundaries before bounding untrusted summary text.
#[must_use]
pub fn one_line(text: &str, max_chars: usize) -> String {
    text.replace(['\r', '\n'], " ")
        .chars()
        .take(max_chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_is_unicode_safe() {
        assert_eq!(truncate("aλ界z", 3), "aλ界…");
        assert_eq!(truncate("aλ", 3), "aλ");
    }

    #[test]
    fn invalid_geometry_produces_no_label() {
        assert!(truncate_to_width("label", f32::NAN, 12.0).is_empty());
        assert!(truncate_to_width("label", 100.0, 0.0).is_empty());
    }
}
