//! Canvas submission and text-resource helpers.

use std::sync::Mutex;

use brood_graphics::{Color, Packet};

use crate::brood::plugin::host;

pub use brood_graphics::{Command, PacketError, Rect, theme};

static DEFERRED_RESOURCE_DROPS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

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

/// Encode and submit a packet for one synchronized resize epoch.
///
/// Unlike [`submit_packet`], the host stages this packet until the matching
/// resize transaction is presented or discarded.
pub fn submit_tagged_packet(pane: u64, resize_epoch: u64, packet: &Packet) -> Result<u64, String> {
    let bytes = packet
        .encode()
        .map_err(|error| format!("encode tagged plugin draw packet: {error}"))?;
    host::graphics_submit_tagged_draw_packet(pane, resize_epoch, &bytes)
        .map_err(|error| format!("submit tagged plugin draw packet: {error:?}"))
}

/// Submit a replacement packet for an ordinary draw or synchronized resize.
///
/// Resize epochs can be superseded while a guest is shaping text or building
/// its packet. An inactive epoch is an expected race, so retry that packet as
/// an ordinary draw. The host either presents it immediately or defers it
/// behind the newer active resize. All other tagged-submit errors remain
/// failures.
pub fn submit_packet_for_resize(
    pane: u64,
    resize_epoch: Option<u64>,
    packet: &Packet,
) -> Result<u64, String> {
    let bytes = packet
        .encode()
        .map_err(|error| format!("encode plugin draw packet: {error}"))?;
    submit_encoded_packet_for_resize(pane, resize_epoch, &bytes)
}

/// Submit an already encoded replacement packet with stale-epoch recovery.
pub fn submit_encoded_packet_for_resize(
    pane: u64,
    resize_epoch: Option<u64>,
    bytes: &[u8],
) -> Result<u64, String> {
    submit_with_resize_recovery(
        resize_epoch,
        || host::graphics_submit_draw_packet(pane, bytes),
        |resize_epoch| host::graphics_submit_tagged_draw_packet(pane, resize_epoch, bytes),
    )
}

fn submit_with_resize_recovery(
    resize_epoch: Option<u64>,
    mut submit_ordinary: impl FnMut() -> Result<u64, host::GraphicsError>,
    submit_tagged: impl FnOnce(u64) -> Result<u64, host::GraphicsError>,
) -> Result<u64, String> {
    let Some(resize_epoch) = resize_epoch else {
        return submit_ordinary().map_err(|error| format!("submit plugin draw packet: {error:?}"));
    };
    match submit_tagged(resize_epoch) {
        Ok(sequence) => Ok(sequence),
        Err(host::GraphicsError::InvalidArgument) => submit_ordinary().map_err(|error| {
            format!("submit plugin draw packet after stale resize epoch: {error:?}")
        }),
        Err(error) => Err(format!("submit tagged plugin draw packet: {error:?}")),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn stale_resize_epoch_retries_as_an_ordinary_draw() {
        let ordinary_calls = Cell::new(0);
        let tagged_epoch = Cell::new(0);
        let result = submit_with_resize_recovery(
            Some(41),
            || {
                ordinary_calls.set(ordinary_calls.get() + 1);
                Ok(9)
            },
            |epoch| {
                tagged_epoch.set(epoch);
                Err(host::GraphicsError::InvalidArgument)
            },
        );

        assert_eq!(result, Ok(9));
        assert_eq!(tagged_epoch.get(), 41);
        assert_eq!(ordinary_calls.get(), 1);
    }

    #[test]
    fn non_epoch_errors_remain_failures() {
        let ordinary_calls = Cell::new(0);
        let result = submit_with_resize_recovery(
            Some(41),
            || {
                ordinary_calls.set(ordinary_calls.get() + 1);
                Ok(9)
            },
            |_epoch| Err(host::GraphicsError::InvalidPacket),
        );

        assert!(result.is_err());
        assert_eq!(ordinary_calls.get(), 0);
    }
}

/// Mark one synchronized resize ready using the pane's current visible content.
pub fn reuse_pane_content(pane: u64, resize_epoch: u64) -> Result<(), String> {
    host::graphics_reuse_pane_content(pane, resize_epoch)
        .map_err(|error| format!("reuse plugin pane content: {error:?}"))
}

/// Best-effort release of resources no longer referenced by the active packet.
///
/// Resources still held by visible or staged content are retried on the next
/// call, after a synchronized resize has had an opportunity to promote.
pub fn drop_resources(pane: u64, resources: impl IntoIterator<Item = u64>) {
    let mut pending = DEFERRED_RESOURCE_DROPS.lock().map_or_else(
        |_poisoned| Vec::new(),
        |mut pending| std::mem::take(&mut *pending),
    );
    pending.extend(resources.into_iter().map(|resource| (pane, resource)));

    let mut deferred = Vec::new();
    for (pane, resource) in pending {
        if matches!(
            host::graphics_drop_resource(pane, resource),
            Err(host::GraphicsError::ResourceInUse)
        ) {
            deferred.push((pane, resource));
        }
    }
    if let Ok(mut pending) = DEFERRED_RESOURCE_DROPS.lock() {
        pending.extend(deferred);
    }
}
