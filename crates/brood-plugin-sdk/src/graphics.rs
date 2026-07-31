//! Canvas submission and text-resource helpers.

use brood_graphics::{Color, Packet};

use crate::brood::plugin::host;

pub use brood_graphics::{Command, PacketError, Rect, theme};

/// Construct the common UI text shape while keeping width bounded.
#[must_use]
pub const fn ui_text_options(
    font_size: f32,
    max_width: f32,
    line_height: f32,
    color: Color,
) -> host::TextLayoutOptions {
    host::TextLayoutOptions {
        font_intent: host::FontIntent::Ui,
        font_weight: host::FontWeight::Normal,
        font_style: host::FontStyle::Normal,
        font_size,
        max_width: if max_width > 1.0 { max_width } else { 1.0 },
        line_height,
        red: color.r,
        green: color.g,
        blue: color.b,
        alpha: color.a,
    }
}

/// Shape one bounded UI label.
pub fn layout_ui_text(
    pane: u64,
    text: &str,
    font_size: f32,
    max_width: f32,
    line_height: f32,
    color: Color,
) -> Result<host::TextLayoutResult, String> {
    host::graphics_layout_text(
        pane,
        text,
        ui_text_options(font_size, max_width, line_height, color),
    )
    .map_err(|error| format!("layout plugin text: {error:?}"))
}

/// Encode and submit a complete packet.
///
/// A plugin should submit the replacement packet before dropping resources
/// referenced by the previous packet.
pub fn submit_packet(pane: u64, packet: &Packet) -> Result<u64, String> {
    let bytes = packet
        .encode()
        .map_err(|error| format!("encode plugin draw packet: {error}"))?;
    host::graphics_submit_draw_packet(pane, &bytes)
        .map_err(|error| format!("submit plugin draw packet: {error:?}"))
}

/// Best-effort release of resources no longer referenced by the active packet.
pub fn drop_resources(pane: u64, resources: impl IntoIterator<Item = u64>) {
    for resource in resources {
        let _drop_result = host::graphics_drop_resource(pane, resource);
    }
}
