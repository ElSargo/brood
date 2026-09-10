//! Shared visual tokens for Brood's native plugin surfaces.

use crate::Color;

/// Brood's default palette, derived directly from Helix's Ashen theme.
///
/// Plugins map these palette values to local semantic roles, but should not
/// introduce independent default color schemes.
pub mod ashen {
    use super::Color;

    pub const BACKGROUND_RGB: u32 = 0x0012_1212;
    pub const TEXT_RGB: u32 = 0x00b4_b4b4;
    pub const CURSOR_RGB: u32 = 0x00c4_693d;
    pub const SELECTION_RGB: u32 = 0x0032_2119;
    pub const ERROR_RGB: u32 = 0x00c5_3030;

    pub const BACKGROUND: Color = rgb(0x12, 0x12, 0x12);
    pub const CURSOR_LINE: Color = rgb(0x19, 0x19, 0x19);
    pub const SURFACE: Color = rgb(0x1d, 0x1d, 0x1d);
    pub const SURFACE_RAISED: Color = rgb(0x21, 0x21, 0x21);
    pub const SURFACE_SUNKEN: Color = rgb(0x15, 0x15, 0x15);
    pub const INTERACTIVE_HOVER: Color = rgb(0x29, 0x29, 0x29);
    pub const INTERACTIVE_PRESSED: Color = rgb(0x3b, 0x2a, 0x22);
    pub const SELECTION: Color = rgb(0x32, 0x21, 0x19);
    pub const BORDER: Color = rgb(0x53, 0x53, 0x53);

    pub const TEXT: Color = rgb(0xb4, 0xb4, 0xb4);
    pub const TEXT_STRONG: Color = rgb(0xd5, 0xd5, 0xd5);
    pub const TEXT_BRIGHT: Color = rgb(0xe5, 0xe5, 0xe5);
    pub const MUTED: Color = rgb(0x73, 0x73, 0x73);
    pub const SUBTLE: Color = rgb(0x32, 0x32, 0x32);

    pub const ACCENT: Color = rgb(0xc4, 0x69, 0x3d);
    pub const ACCENT_GLOW: Color = rgb(0xd8, 0x7c, 0x4a);
    pub const ACCENT_MUTED: Color = rgb(0x6d, 0x3b, 0x22);
    pub const WARNING: Color = rgb(0xe5, 0xa7, 0x2a);
    pub const INFO: Color = rgb(0x4a, 0x8b, 0x8b);
    pub const DANGER: Color = rgb(0xb1, 0x42, 0x42);
    pub const ERROR: Color = rgb(0xc5, 0x30, 0x30);
    pub const ERROR_BRIGHT: Color = rgb(0xdf, 0x64, 0x64);

    pub const DIFF_ADDED: Color = MUTED;
    pub const DIFF_DELETED: Color = DANGER;
    pub const DIFF_MODIFIED: Color = rgb(0x89, 0x49, 0x2a);
    pub const DIFF_ADDED_BG: Color = rgba(0x73, 0x73, 0x73, 0.16);
    pub const DIFF_DELETED_BG: Color = rgba(0xb1, 0x42, 0x42, 0.16);
    pub const DIFF_EMPTY_BG: Color = rgba(0x15, 0x15, 0x15, 0.72);

    /// ANSI colors used when a terminal application does not provide RGB.
    pub const ANSI_NORMAL: [u32; 8] = [
        0x001d_1d1d,
        0x00b1_4242,
        0x0073_7373,
        0x00e5_a72a,
        0x004a_8b8b,
        0x00d8_7c4a,
        0x0089_492a,
        0x00b4_b4b4,
    ];
    pub const ANSI_BRIGHT: [u32; 8] = [
        0x0053_5353,
        0x00df_6464,
        0x0094_9494,
        0x00e4_9a44,
        0x004a_8b8b,
        0x00c4_693d,
        0x00d8_7c4a,
        0x00e5_e5e5,
    ];

    const fn rgb(red: u8, green: u8, blue: u8) -> Color {
        rgba(red, green, blue, 1.0)
    }

    const fn rgba(red: u8, green: u8, blue: u8, alpha: f32) -> Color {
        Color::rgba(
            red as f32 / 255.0,
            green as f32 / 255.0,
            blue as f32 / 255.0,
            alpha,
        )
    }
}

/// Resolved Helix style. `None` means inherit the corresponding base field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RoleStyle {
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    pub modifiers: Option<u16>,
    pub underline_color: Option<u32>,
    pub underline_style: Option<UnderlineStyle>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UnderlineStyle {
    Line,
    Curl,
    Dashed,
    Dotted,
    DoubleLine,
}

impl RoleStyle {
    #[must_use]
    pub fn over(self, base: Self) -> Self {
        Self {
            fg: self.fg.or(base.fg),
            bg: self.bg.or(base.bg),
            modifiers: match (self.modifiers, base.modifiers) {
                (Some(a), Some(b)) => Some(a | b),
                (a, b) => a.or(b),
            },
            underline_color: self.underline_color.or(base.underline_color),
            underline_style: self.underline_style.or(base.underline_style),
        }
    }
}

/// Explicit immutable render configuration, bounded independently of application data.
/// Palette identifiers never cross this interface.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThemeSnapshot {
    pub revision: u64,
    pub name: String,
    pub roles: std::collections::BTreeMap<String, RoleStyle>,
}

pub const MAX_THEME_BYTES: usize = 256 * 1024;
pub const MAX_THEME_ROLES: usize = 2048;
pub const BOLD: u16 = 1;
pub const DIM: u16 = 2;
pub const ITALIC: u16 = 4;
pub const SLOW_BLINK: u16 = 8;
pub const RAPID_BLINK: u16 = 16;
pub const REVERSED: u16 = 32;
pub const HIDDEN: u16 = 64;
pub const CROSSED_OUT: u16 = 128;

impl Default for ThemeSnapshot {
    fn default() -> Self {
        Self {
            revision: 1,
            name: "default".into(),
            roles: std::collections::BTreeMap::from([
                (
                    "ui.text".into(),
                    RoleStyle {
                        fg: Some(ashen::TEXT_RGB),
                        ..RoleStyle::default()
                    },
                ),
                (
                    "ui.background".into(),
                    RoleStyle {
                        bg: Some(ashen::BACKGROUND_RGB),
                        ..RoleStyle::default()
                    },
                ),
            ]),
        }
    }
}

impl ThemeSnapshot {
    /// Longest dot-delimited scope match; a present partial style is not filled
    /// from its parent. Call `over` to compose it with an application base.
    #[must_use]
    pub fn style(&self, mut scope: &str) -> RoleStyle {
        loop {
            if let Some(style) = self.roles.get(scope) {
                return *style;
            }
            let Some((parent, _)) = scope.rsplit_once('.') else {
                return RoleStyle::default();
            };
            scope = parent;
        }
    }

    #[must_use]
    pub fn fg(&self, scope: &str) -> Color {
        rgb_color(
            self.style(scope)
                .fg
                .or_else(|| self.style("ui.text").fg)
                .unwrap_or(ashen::TEXT_RGB),
        )
    }
    #[must_use]
    pub fn bg(&self, scope: &str) -> Color {
        rgb_color(
            self.style(scope)
                .bg
                .or_else(|| self.style("ui.background").bg)
                .unwrap_or(ashen::BACKGROUND_RGB),
        )
    }
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_THEME_BYTES {
            return Err("theme configuration exceeds byte limit".into());
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        // Empty configuration is the legacy facet caller's default.
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        if bytes.len() > MAX_THEME_BYTES {
            return Err("theme configuration exceeds byte limit".into());
        }
        let snapshot: Self = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        snapshot.validate()?;
        Ok(snapshot)
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.revision == 0 || self.name.len() > 256 || self.roles.len() > MAX_THEME_ROLES {
            return Err("invalid theme revision, name or role count".into());
        }
        for (scope, style) in &self.roles {
            if scope.is_empty()
                || scope.len() > 256
                || [style.fg, style.bg, style.underline_color]
                    .into_iter()
                    .flatten()
                    .any(|rgb| rgb > 0xff_ffff)
                || style.modifiers.is_some_and(|bits| bits > 255)
            {
                return Err(format!("invalid theme role {scope:?}"));
            }
        }
        Ok(())
    }
}

#[must_use]
pub const fn rgb_color(rgb: u32) -> Color {
    let [_, red, green, blue] = rgb.to_be_bytes();
    Color::rgba(
        red as f32 / 255.0,
        green as f32 / 255.0,
        blue as f32 / 255.0,
        1.0,
    )
}

/// Terminal snapshot colors reserve the high byte for semantic defaults.
/// Indexed colors retain the terminal's explicit ANSI policy; RGB is unchanged.
pub const TERMINAL_DEFAULT_FG: u32 = 0x0100_0000;
pub const TERMINAL_DEFAULT_BG: u32 = 0x0200_0000;
impl ThemeSnapshot {
    #[must_use]
    pub fn terminal_color(&self, value: u32) -> Color {
        match value {
            TERMINAL_DEFAULT_FG => self.fg("ui.text"),
            TERMINAL_DEFAULT_BG => self.bg("ui.background"),
            rgb => rgb_color(rgb),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_defaults_are_distinct_from_matching_truecolor() {
        let mut light = ThemeSnapshot::default();
        light.roles.insert(
            "ui.text".into(),
            RoleStyle {
                fg: Some(0x0012_3456),
                ..RoleStyle::default()
            },
        );
        assert_eq!(
            light.terminal_color(TERMINAL_DEFAULT_FG),
            rgb_color(0x0012_3456)
        );
        assert_eq!(light.terminal_color(ashen::TEXT_RGB), ashen::TEXT);
        assert_eq!(light.terminal_color(0x00fe_dcba), rgb_color(0x00fe_dcba));
    }

    #[test]
    fn snapshot_bounds_and_omitted_fields_are_preserved() {
        let mut theme = ThemeSnapshot::default();
        theme.roles.insert(
            "ui.text.focus".into(),
            RoleStyle {
                modifiers: Some(BOLD),
                ..RoleStyle::default()
            },
        );
        assert_eq!(theme.style("ui.text.focus.child").fg, None);
        assert_eq!(theme.style("ui.text.other").fg, Some(ashen::TEXT_RGB));
        let bytes = theme.encode().unwrap();
        assert_eq!(ThemeSnapshot::decode(&bytes).unwrap(), theme);
        theme.revision = 0;
        assert!(theme.encode().is_err());
        assert!(ThemeSnapshot::decode(&vec![0; MAX_THEME_BYTES + 1]).is_err());
    }
}
