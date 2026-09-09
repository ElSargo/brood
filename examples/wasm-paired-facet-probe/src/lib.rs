use brood_graphics::{Color, Command, Packet, Rect};
use brood_plugin_sdk::brood::plugin::host;
use brood_plugin_sdk::exports::brood::plugin::actor::{
    Command as ActorCommand, Event, Guest as ActorGuest, WorkerId,
};
use brood_plugin_sdk::{Facets, export_visual_plugin};
use brood_protocol::{
    facet_codec,
    facets::{HostRequest, InputEvent, InputResponse, PaneContent, PaneOutput, RenderResponse},
};
use core::sync::atomic::{AtomicU32, Ordering};

static CONSTRUCTOR_PROBES: AtomicU32 = AtomicU32::new(0);

#[cfg(target_arch = "wasm32")]
extern "C" fn probe_denied_constructor_graphics() {
    assert_eq!(
        host::graphics_create_image_rgba8(99, 1, 1, &[0, 0, 0, 0]),
        Err(host::GraphicsError::Unavailable)
    );
    assert!(matches!(
        host::graphics_layout_text(
            99,
            "constructor",
            host::TextLayoutOptions {
                font_intent: host::FontIntent::Ui,
                font_weight: host::FontWeight::Normal,
                font_style: host::FontStyle::Normal,
                font_size: 12.0,
                max_width: 100.0,
                line_height: 14.0,
                red: 1.0,
                green: 1.0,
                blue: 1.0,
                alpha: 1.0,
            },
        ),
        Err(host::GraphicsError::Unavailable)
    ));
    assert_eq!(
        host::graphics_drop_resource(99, 999),
        Err(host::GraphicsError::Unavailable)
    );
    CONSTRUCTOR_PROBES.fetch_add(1, Ordering::Relaxed);
}

#[cfg(target_arch = "wasm32")]
#[used]
#[unsafe(link_section = ".init_array")]
static GRAPHICS_CONSTRUCTOR_PROBE: extern "C" fn() = probe_denied_constructor_graphics;

struct PairedFacet;

impl ActorGuest for PairedFacet {
    fn dispatch(event: Event) -> Vec<ActorCommand> {
        match event {
            Event::FacetInput(batch) => {
                let selected = batch
                    .events
                    .first()
                    .and_then(|event| event.as_slice().try_into().ok())
                    .map(u64::from_le_bytes);
                let mut result = b"facet-input-rejected:".to_vec();
                result.extend_from_slice(&selected.unwrap_or_default().to_le_bytes());
                result.extend_from_slice(&batch.snapshot_revision.to_le_bytes());
                vec![ActorCommand::SendHostMessage(result)]
            }
            _ => Vec::new(),
        }
    }
    async fn worker_run(_worker: WorkerId, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(payload)
    }
}

impl Facets for PairedFacet {
    fn render(input: Vec<u8>) -> Vec<u8> {
        assert_eq!(CONSTRUCTOR_PROBES.load(Ordering::Relaxed), 1);
        let decode = facet_codec::DecodeLimits {
            max_wire_bytes: input.len(),
            max_allocated_bytes: 4 * 1024 * 1024,
            max_allocations: 4096,
        };
        let request =
            facet_codec::decode_render_request(&input, decode).expect("valid render request");
        if request.configuration == b"trap" {
            panic!("requested facet trap");
        }
        if request.configuration == b"no-change" {
            let response = RenderResponse {
                token: request.token,
                panes: request
                    .panes
                    .into_iter()
                    .map(|pane| PaneOutput {
                        pane: pane.pane,
                        content: PaneContent::NoChange,
                    })
                    .collect(),
                next_frame_ns: None,
            };
            return encode_render(&response, request.limits.max_output_bytes as usize);
        }
        if request.configuration == b"graphics-probe" {
            let pane = request.panes[0].pane;
            let image = host::graphics_create_image_rgba8(pane, 1, 1, &[255, 0, 0, 255])
                .expect("image permission");
            let layout = host::graphics_layout_text(
                pane,
                "probe",
                host::TextLayoutOptions {
                    font_intent: host::FontIntent::Ui,
                    font_weight: host::FontWeight::Normal,
                    font_style: host::FontStyle::Normal,
                    font_size: 12.0,
                    max_width: 100.0,
                    line_height: 14.0,
                    red: 1.0,
                    green: 1.0,
                    blue: 1.0,
                    alpha: 1.0,
                },
            )
            .expect("text permission");
            host::graphics_drop_resource(pane, layout.handle).expect("temporary drop permission");
            let mut packet = Packet::new();
            packet.push(Command::DrawImage {
                image,
                destination: Rect::new(0.0, 0.0, 1.0, 1.0),
            });
            let response = RenderResponse {
                token: request.token,
                panes: vec![PaneOutput {
                    pane,
                    content: PaneContent::Draw {
                        packet: packet.encode().expect("valid resource packet"),
                        interaction: Vec::new(),
                    },
                }],
                next_frame_ns: None,
            };
            return encode_render(&response, request.limits.max_output_bytes as usize);
        }
        let ids = decode_ids(&request.application_view);
        let mut panes = Vec::with_capacity(request.panes.len());
        for target in &request.panes {
            let mut packet = Packet::new();
            for (row, id) in ids.iter().enumerate() {
                let shade = ((*id % 191) as f32 + 32.0) / 255.0;
                packet.push(Command::FillRect {
                    rect: Rect {
                        x: 0.0,
                        y: row as f32 * 20.0,
                        width: target.logical_width as f32,
                        height: 18.0,
                    },
                    color: Color {
                        r: shade,
                        g: 0.25,
                        b: 0.5,
                        a: 1.0,
                    },
                });
            }
            let mut interaction = Vec::with_capacity(16 + ids.len() * 8);
            interaction.extend_from_slice(&request.token.invocation.to_le_bytes());
            interaction.extend_from_slice(&request.token.snapshot_revision.to_le_bytes());
            for id in &ids {
                interaction.extend_from_slice(&id.to_le_bytes());
            }
            panes.push(PaneOutput {
                pane: target.pane,
                content: PaneContent::Draw {
                    packet: packet.encode().expect("valid packet"),
                    interaction,
                },
            });
        }
        let response = RenderResponse {
            token: request.token,
            panes,
            next_frame_ns: None,
        };
        encode_render(&response, request.limits.max_output_bytes as usize)
    }

    fn interpret(input: Vec<u8>) -> Vec<u8> {
        assert_eq!(CONSTRUCTOR_PROBES.load(Ordering::Relaxed), 1);
        let decode = facet_codec::DecodeLimits {
            max_wire_bytes: input.len(),
            max_allocated_bytes: 4 * 1024 * 1024,
            max_allocations: 4096,
        };
        let request =
            facet_codec::decode_input_request(&input, decode).expect("valid input request");
        if matches!(&request.event, InputEvent::Text { text } if text == "graphics-denied") {
            assert_eq!(
                host::graphics_create_image_rgba8(request.geometry.pane, 1, 1, &[0, 0, 0, 0]),
                Err(host::GraphicsError::Unavailable)
            );
            assert!(matches!(
                host::graphics_layout_text(
                    request.geometry.pane,
                    "denied",
                    host::TextLayoutOptions {
                        font_intent: host::FontIntent::Ui,
                        font_weight: host::FontWeight::Normal,
                        font_style: host::FontStyle::Normal,
                        font_size: 12.0,
                        max_width: 100.0,
                        line_height: 14.0,
                        red: 1.0,
                        green: 1.0,
                        blue: 1.0,
                        alpha: 1.0,
                    },
                ),
                Err(host::GraphicsError::Unavailable)
            ));
            assert_eq!(
                host::graphics_drop_resource(request.geometry.pane, 999),
                Err(host::GraphicsError::Unavailable)
            );
        }
        let ids = decode_ids(request.interaction.get(16..).unwrap_or_default());
        let selected = match request.event {
            InputEvent::PointerButton {
                y, pressed: true, ..
            } => ids.get((y.max(0.0) / 20.0) as usize).copied(),
            _ => None,
        };
        let events = selected
            .map(|id| vec![id.to_le_bytes().to_vec()])
            .unwrap_or_default();
        let response = InputResponse {
            token: request.token,
            next_sequence_state: selected
                .map(u64::to_le_bytes)
                .map(Vec::from)
                .unwrap_or(request.sequence_state),
            events,
            requests: if selected.is_some() {
                vec![HostRequest::Render]
            } else {
                Vec::new()
            },
        };
        encode_input(&response, request.limits.max_output_bytes as usize)
    }
}

fn decode_ids(bytes: &[u8]) -> Vec<u64> {
    bytes
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().expect("eight-byte ID")))
        .collect()
}
fn encode_render(value: &RenderResponse, max: usize) -> Vec<u8> {
    let mut output = vec![0; max];
    let len =
        facet_codec::encode_render_response(value, &mut output).expect("bounded render response");
    output.truncate(len);
    output
}
fn encode_input(value: &InputResponse, max: usize) -> Vec<u8> {
    let mut output = vec![0; max];
    let len =
        facet_codec::encode_input_response(value, &mut output).expect("bounded input response");
    output.truncate(len);
    output
}

export_visual_plugin!(PairedFacet);
