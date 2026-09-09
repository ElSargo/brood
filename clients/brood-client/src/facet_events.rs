//! Conversion of client presentation input into the facet input protocol.

use brood_protocol::facets::InputEvent;
use brood_wasm::presentation::PresentationEvent;

/// Convert an input event while preserving every user supplied field and
/// moving owned strings and buffers into the facet request.
///
/// Lifecycle and scheduling events are consumed by the client authority and
/// therefore cannot be represented as facet input.
pub fn into_facet_input(event: PresentationEvent) -> Result<(u64, InputEvent), PresentationEvent> {
    match event {
        PresentationEvent::PointerMoved { pane, x, y } => Ok((
            pane,
            InputEvent::PointerMotion {
                x: f64::from(x),
                y: f64::from(y),
            },
        )),
        PresentationEvent::PointerLeft { pane } => Ok((pane, InputEvent::PointerLeave)),
        PresentationEvent::PointerButton {
            pane,
            x,
            y,
            button,
            pressed,
            modifiers,
        } => Ok((
            pane,
            InputEvent::PointerButton {
                x: f64::from(x),
                y: f64::from(y),
                button,
                pressed,
                modifiers,
            },
        )),
        PresentationEvent::PointerScroll {
            pane,
            x,
            y,
            horizontal,
            vertical,
            modifiers,
        } => Ok((
            pane,
            InputEvent::PointerScroll {
                x: f64::from(x),
                y: f64::from(y),
                horizontal: f64::from(horizontal),
                vertical: f64::from(vertical),
                modifiers,
            },
        )),
        PresentationEvent::Key {
            pane,
            physical_key,
            logical_key,
            pressed,
            repeat,
            modifiers,
        } => Ok((
            pane,
            InputEvent::Key {
                code: physical_key,
                logical_key,
                modifiers,
                pressed,
                repeat,
            },
        )),
        PresentationEvent::Text { pane, text } => Ok((pane, InputEvent::Text { text })),
        PresentationEvent::Paste { pane, text } => Ok((pane, InputEvent::Paste { text })),
        PresentationEvent::Image {
            pane,
            name,
            mime_type,
            bytes,
            preview_width,
            preview_height,
            preview_rgba8,
        } => Ok((
            pane,
            InputEvent::Image {
                name,
                mime_type,
                bytes,
                preview_width,
                preview_height,
                preview_rgba8,
            },
        )),
        lifecycle @ (PresentationEvent::PaneCreated { .. }
        | PresentationEvent::PaneResized { .. }
        | PresentationEvent::PaneFocused { .. }
        | PresentationEvent::RenderHint(..)) => Err(lifecycle),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, ensure};

    #[test]
    fn preserves_key_text_and_image_payloads() -> Result<()> {
        let key_result = into_facet_input(PresentationEvent::Key {
            pane: 7,
            physical_key: 42,
            logical_key: "λ".into(),
            pressed: true,
            repeat: true,
            modifiers: 9,
        });
        let (pane, key) = key_result.map_err(|_event| anyhow::anyhow!("key conversion failed"))?;
        ensure!(pane == 7);
        ensure!(
            key == InputEvent::Key {
                code: 42,
                logical_key: "λ".into(),
                modifiers: 9,
                pressed: true,
                repeat: true,
            }
        );

        let image = PresentationEvent::Image {
            pane: 8,
            name: "preview".into(),
            mime_type: "image/png".into(),
            bytes: vec![1, 2, 3],
            preview_width: 2,
            preview_height: 1,
            preview_rgba8: vec![4, 5, 6, 7, 8, 9, 10, 11],
        };
        let (image_pane, converted) =
            into_facet_input(image).map_err(|_event| anyhow::anyhow!("image conversion failed"))?;
        ensure!(image_pane == 8);
        ensure!(
            converted
                == InputEvent::Image {
                    name: "preview".into(),
                    mime_type: "image/png".into(),
                    bytes: vec![1, 2, 3],
                    preview_width: 2,
                    preview_height: 1,
                    preview_rgba8: vec![4, 5, 6, 7, 8, 9, 10, 11],
                }
        );
        Ok(())
    }

    #[test]
    fn lifecycle_events_are_returned_unchanged() -> Result<()> {
        let event = PresentationEvent::PaneResized {
            pane: 3,
            width: 10.0,
            height: 20.0,
            scale: 1.5,
            resize_epoch: Some(12),
        };
        ensure!(into_facet_input(event.clone()) == Err(event));

        let hint = PresentationEvent::RenderHint(brood_wasm::presentation::RenderHint {
            token: brood_wasm::presentation::RenderToken(4),
            target_presentation_time_ns: 5,
            submission_deadline_ns: 6,
            relevant_panes: vec![1, 2],
            event_watermark: 5,
        });
        ensure!(into_facet_input(hint.clone()) == Err(hint));
        Ok(())
    }
}
