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
