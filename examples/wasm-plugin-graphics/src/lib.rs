wit_bindgen::generate!({
    path: "../../crates/brood-wasm/wit",
    world: "brood-plugin",
});

use std::sync::Mutex;
#[cfg(not(feature = "pure-renderer"))]
use std::time::Duration;

#[cfg(not(feature = "pure-renderer"))]
use brood::plugin::host;
use brood_graphics::{BlendMode, Color, Command, Packet, Path, Rect, Transform};
#[cfg(feature = "pure-renderer")]
use brood_plugin_sdk::brood::plugin::host;
#[cfg(feature = "pure-renderer")]
use brood_plugin_sdk::exports::brood::plugin::actor::{
    Command as ActorCommand, Event, Guest, LifecycleEvent, PresentationEvent,
};
#[cfg(feature = "pure-renderer")]
use brood_plugin_sdk::{Facets, export_visual_plugin, facet_codec, facets};
#[cfg(not(feature = "pure-renderer"))]
use exports::brood::plugin::actor::{Command as ActorCommand, Event, Guest, PresentationEvent};
#[cfg(feature = "pure-renderer")]
use serde::{Deserialize, Serialize};

const LEFT_BUTTON: u32 = 0x110;

#[cfg(not(feature = "pure-renderer"))]
#[derive(Clone, Copy)]
struct DemoPane {
    pane: u64,
    width: f32,
    height: f32,
    image: u64,
    text_layout: u64,
    retired_text_layout: u64,
    pending_resize_epoch: Option<u64>,
    pointer_x: f32,
    pointer_y: f32,
}

#[cfg(not(feature = "pure-renderer"))]
struct DemoState {
    panes: Vec<DemoPane>,
    phase: f32,
    active_palette: bool,
    subscribed: bool,
}

#[cfg(not(feature = "pure-renderer"))]
impl DemoState {
    const fn new() -> Self {
        Self {
            panes: Vec::new(),
            phase: 0.0,
            active_palette: false,
            subscribed: false,
        }
    }
}

#[cfg(not(feature = "pure-renderer"))]
static STATE: Mutex<DemoState> = Mutex::new(DemoState::new());

#[cfg(feature = "pure-renderer")]
#[derive(Clone, Copy, Deserialize, Serialize)]
struct RenderView {
    phase: f32,
    active_palette: bool,
    animated: bool,
}

#[cfg(feature = "pure-renderer")]
impl RenderView {
    const fn new() -> Self {
        Self {
            phase: 0.0,
            active_palette: false,
            animated: false,
        }
    }
}

#[cfg(feature = "pure-renderer")]
#[derive(Default, Deserialize, Serialize)]
struct SequenceState {
    pointers: Vec<(u64, f32, f32)>,
    pressed_pane: Option<u64>,
}

#[cfg(feature = "pure-renderer")]
#[derive(Deserialize, Serialize)]
struct InteractionMap {
    pane: u64,
    phase: f32,
    animated: bool,
}

#[cfg(feature = "pure-renderer")]
#[derive(Debug, PartialEq, Deserialize, Serialize)]
enum FacetIntent {
    TogglePalette,
    AdvancePhase(f32),
    SetAnimation { animated: bool, phase: f32 },
}

#[cfg(feature = "pure-renderer")]
struct PureState {
    view: RenderView,
    revision: u64,
    published: Vec<u8>,
}

#[cfg(feature = "pure-renderer")]
impl PureState {
    const fn new() -> Self {
        Self {
            view: RenderView::new(),
            revision: 0,
            published: Vec::new(),
        }
    }
}

#[cfg(feature = "pure-renderer")]
static PURE_STATE: Mutex<PureState> = Mutex::new(PureState::new());

struct GraphicsDemo;

#[cfg(not(feature = "pure-renderer"))]
impl Guest for GraphicsDemo {
    fn dispatch(event: Event) -> Vec<exports::brood::plugin::actor::Command> {
        if matches!(event, Event::Lifecycle(_)) {
            return vec![ActorCommand::LifecycleComplete(Ok(()))];
        }
        let Event::Presentation(event) = event else {
            return Vec::new();
        };
        let Ok(mut state) = STATE.lock() else {
            return Vec::new();
        };
        let redraw = match event {
            PresentationEvent::PaneCreated(created) => {
                if state.panes.len() >= 16
                    || state.panes.iter().any(|pane| pane.pane == created.pane)
                {
                    return Vec::new();
                }
                let width = created.width.max(1.0);
                let height = created.height.max(1.0);
                state.panes.push(DemoPane {
                    pane: created.pane,
                    width,
                    height,
                    image: create_checker_image(created.pane).unwrap_or_default(),
                    text_layout: create_title_layout(created.pane, width).unwrap_or_default(),
                    retired_text_layout: 0,
                    pending_resize_epoch: None,
                    pointer_x: width * 0.5,
                    pointer_y: height * 0.5,
                });
                true
            }
            PresentationEvent::PaneResized(resized) => {
                let Some(pane) = state
                    .panes
                    .iter_mut()
                    .find(|pane| pane.pane == resized.pane)
                else {
                    return Vec::new();
                };
                pane.width = resized.width.max(1.0);
                pane.height = resized.height.max(1.0);
                pane.pending_resize_epoch = resized.resize_epoch;
                if let Ok(layout) = create_title_layout(pane.pane, pane.width) {
                    pane.retired_text_layout = pane.text_layout;
                    pane.text_layout = layout;
                }
                true
            }
            PresentationEvent::PointerMoved(pointer) => {
                let Some(pane) = state
                    .panes
                    .iter_mut()
                    .find(|pane| pane.pane == pointer.pane)
                else {
                    return Vec::new();
                };
                pane.pointer_x = pointer.x;
                pane.pointer_y = pointer.y;
                true
            }
            PresentationEvent::PointerButton(button)
                if state.panes.iter().any(|pane| pane.pane == button.pane)
                    && button.button == LEFT_BUTTON
                    && button.pressed =>
            {
                state.active_palette = !state.active_palette;
                state.phase += 0.35;
                true
            }
            PresentationEvent::PointerScroll(scroll)
                if state.panes.iter().any(|pane| pane.pane == scroll.pane) =>
            {
                state.phase += scroll.vertical * 0.08;
                true
            }
            PresentationEvent::Key(key)
                if state.panes.iter().any(|pane| pane.pane == key.pane)
                    && key.logical_key == "F8"
                    && key.pressed
                    && !key.repeat =>
            {
                let subscribed = !state.subscribed;
                if host::render_set_subscription(subscribed).is_ok() {
                    state.subscribed = subscribed;
                }
                false
            }
            PresentationEvent::Key(key)
                if state.panes.iter().any(|pane| pane.pane == key.pane)
                    && key.pressed
                    && !key.repeat =>
            {
                state.phase += 0.35;
                true
            }
            PresentationEvent::Text(text)
                if state.panes.iter().any(|pane| pane.pane == text.pane) =>
            {
                state.active_palette = !state.active_palette;
                state.phase += 0.15;
                true
            }
            PresentationEvent::RenderHint(hint) => {
                if state.subscribed {
                    state.phase = Duration::from_nanos(hint.target_presentation_time_ns)
                        .as_secs_f32()
                        * core::f32::consts::TAU;
                }
                let submitted = submit_scene(&state, &hint).is_ok();
                if submitted {
                    for pane in &mut state.panes {
                        if pane.retired_text_layout != 0 {
                            let _ =
                                host::graphics_drop_resource(pane.pane, pane.retired_text_layout);
                            pane.retired_text_layout = 0;
                        }
                        pane.pending_resize_epoch = None;
                    }
                }
                false
            }
            _ => false,
        };
        if redraw {
            let _ = host::render_request();
        }
        Vec::new()
    }

    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

#[cfg(feature = "pure-renderer")]
impl Guest for GraphicsDemo {
    fn dispatch(event: Event) -> Vec<ActorCommand> {
        match event {
            Event::Lifecycle(LifecycleEvent::Start(_))
            | Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![ActorCommand::LifecycleComplete(Ok(()))]
            }
            Event::Presentation(PresentationEvent::PaneCreated(_)) => {
                publish_view();
                Vec::new()
            }
            Event::FacetInput(batch) => {
                let Ok(mut state) = PURE_STATE.lock() else {
                    return Vec::new();
                };
                for bytes in batch.events {
                    match serde_json::from_slice::<FacetIntent>(&bytes) {
                        Ok(FacetIntent::TogglePalette) => {
                            state.view.active_palette = !state.view.active_palette;
                            state.view.phase += 0.35;
                        }
                        Ok(FacetIntent::AdvancePhase(delta)) if delta.is_finite() => {
                            state.view.phase += delta;
                        }
                        Ok(FacetIntent::SetAnimation { animated, phase }) if phase.is_finite() => {
                            state.view.animated = animated;
                            state.view.phase = phase;
                        }
                        Ok(FacetIntent::SetAnimation { .. })
                        | Ok(FacetIntent::AdvancePhase(_))
                        | Err(_) => {}
                    }
                }
                drop(state);
                publish_view();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

#[cfg(feature = "pure-renderer")]
fn publish_view() {
    let Ok(mut state) = PURE_STATE.lock() else {
        return;
    };
    let Ok(bytes) = serde_json::to_vec(&state.view) else {
        return;
    };
    if bytes == state.published {
        return;
    }
    state.revision = state.revision.saturating_add(1).max(1);
    if brood_plugin_sdk::publication::publish(state.revision, &bytes).is_ok() {
        state.published = bytes;
    }
}

fn create_checker_image(pane: u64) -> Result<u64, String> {
    const SIZE: u32 = 64;
    let mut pixels = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let bright = ((x / 8) + (y / 8)) % 2 == 0;
            let (red, green, blue) = if bright {
                (250, 184, 82)
            } else {
                (91, 73, 214)
            };
            pixels.extend_from_slice(&[red, green, blue, 255]);
        }
    }
    host::graphics_create_image_rgba8(pane, SIZE, SIZE, &pixels)
        .map_err(|error| format!("create demo image: {error:?}"))
}

#[cfg(not(feature = "pure-renderer"))]
fn create_title_layout(pane: u64, width: f32) -> Result<u64, String> {
    host::graphics_layout_text(
        pane,
        "Canvas packets, native text — ffi العربية 日本語\nMove, click, scroll, or type to redraw",
        host::TextLayoutOptions {
            font_intent: host::FontIntent::Ui,
            font_weight: host::FontWeight::Normal,
            font_style: host::FontStyle::Normal,
            font_size: 22.0,
            max_width: (width - 48.0).max(120.0),
            line_height: 30.0,
            red: 0.9,
            green: 0.93,
            blue: 0.98,
            alpha: 1.0,
        },
    )
    .map(|layout| layout.handle)
    .map_err(|error| format!("layout demo title: {error:?}"))
}

#[cfg(not(feature = "pure-renderer"))]
fn submit_scene(
    state: &DemoState,
    hint: &exports::brood::plugin::actor::RenderHint,
) -> Result<u64, String> {
    let draws = state
        .panes
        .iter()
        .map(|pane| {
            let packet = build_scene((*pane).into(), state.phase, state.active_palette)
                .encode()
                .map_err(|error| error.to_string())?;
            Ok(host::ActorFrameDraw {
                pane: pane.pane,
                resize_epoch: pane.pending_resize_epoch,
                state_watermark: None,
                packet,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    host::graphics_finish_actor_frame(
        host::RenderToken {
            opaque: hint.token.opaque,
        },
        &draws,
    )
    .map_err(|error| format!("submit demo packet: {error:?}"))
}

#[derive(Clone, Copy)]
struct ScenePane {
    width: f32,
    height: f32,
    image: u64,
    text_layout: u64,
    pointer_x: f32,
    pointer_y: f32,
}

#[cfg(not(feature = "pure-renderer"))]
impl From<DemoPane> for ScenePane {
    fn from(value: DemoPane) -> Self {
        Self {
            width: value.width,
            height: value.height,
            image: value.image,
            text_layout: value.text_layout,
            pointer_x: value.pointer_x,
            pointer_y: value.pointer_y,
        }
    }
}

fn build_scene(state: ScenePane, phase: f32, active_palette: bool) -> Packet {
    let mut packet = Packet::new();
    let width = state.width;
    let height = state.height;
    let cell = (width.min(height) / 5.4).max(68.0);
    let gap = cell * 0.12;
    let margin = cell * 0.22;
    let background = if active_palette {
        Color::rgba(0.025, 0.055, 0.075, 1.0)
    } else {
        Color::rgba(0.035, 0.04, 0.065, 1.0)
    };
    let primary = if active_palette {
        Color::rgba(0.18, 0.88, 0.67, 1.0)
    } else {
        Color::rgba(0.45, 0.36, 0.94, 1.0)
    };
    let accent = if active_palette {
        Color::rgba(1.0, 0.43, 0.31, 1.0)
    } else {
        Color::rgba(0.98, 0.68, 0.23, 1.0)
    };
    let pale = Color::rgba(0.84, 0.89, 0.98, 1.0);

    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, width, height),
        color: background,
    });

    // clip-rect + fill-rect
    packet.push(Command::Save);
    packet.push(Command::ClipRect(Rect::new(margin, margin, cell, cell)));
    packet.push(Command::FillRect {
        rect: Rect::new(margin - cell * 0.2, margin, cell * 1.4, cell),
        color: primary,
    });
    packet.push(Command::Restore);

    // stroke-rect
    packet.push(Command::StrokeRect {
        rect: Rect::new(margin + cell + gap, margin, cell, cell),
        color: pale,
        width: 4.0,
    });

    // rounded rectangles
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(margin + (cell + gap) * 2.0, margin, cell, cell),
        radius: cell * 0.22,
        color: accent,
    });
    packet.push(Command::StrokeRoundedRect {
        rect: Rect::new(margin + (cell + gap) * 3.0, margin, cell, cell),
        radius: cell * 0.3,
        color: primary,
        width: 5.0,
    });

    // fill-path + clip-path
    let second_y = margin + cell + gap;
    let star = star_path(margin + cell * 0.5, second_y + cell * 0.5, cell * 0.46);
    packet.push(Command::FillPath {
        path: star.clone(),
        color: primary,
    });
    packet.push(Command::Save);
    packet.push(Command::ClipPath(star));
    packet.push(Command::FillRect {
        rect: Rect::new(margin, second_y + cell * 0.48, cell, cell * 0.52),
        color: accent,
    });
    packet.push(Command::Restore);

    // stroke-path with quadratic and cubic segments
    packet.push(Command::StrokePath {
        path: Path::new()
            .move_to(margin + cell + gap, second_y + cell * 0.8)
            .quad_to(
                margin + cell * 1.6 + gap,
                second_y - cell * 0.15,
                margin + cell * 2.0 + gap,
                second_y + cell * 0.55,
            )
            .cubic_to(
                (margin + cell * 2.25, second_y + cell),
                (margin + cell * 2.65, second_y + cell * 0.1),
                (margin + cell * 2.1 + gap, second_y + cell * 0.2),
            ),
        color: accent,
        width: 7.0,
    });

    // draw-image
    if state.image != 0 {
        packet.push(Command::DrawImage {
            image: state.image,
            destination: Rect::new(margin + (cell + gap) * 2.0, second_y, cell, cell),
        });
    }

    // set-transform
    let angle = phase;
    let cosine = angle.cos();
    let sine = angle.sin();
    let center_x = margin + (cell + gap) * 3.0 + cell * 0.5;
    let center_y = second_y + cell * 0.5;
    packet.push(Command::Save);
    packet.push(Command::SetTransform(Transform {
        sx: cosine,
        ky: sine,
        kx: -sine,
        sy: cosine,
        tx: center_x - cosine * center_x + sine * center_y,
        ty: center_y - sine * center_x - cosine * center_y,
    }));
    packet.push(Command::FillRect {
        rect: Rect::new(
            center_x - cell * 0.34,
            center_y - cell * 0.34,
            cell * 0.68,
            cell * 0.68,
        ),
        color: primary,
    });
    packet.push(Command::Restore);

    // begin/end-layer + opacity + blend mode
    let third_y = second_y + cell + gap;
    packet.push(Command::BeginLayer);
    packet.push(Command::Save);
    packet.push(Command::SetOpacity(0.72));
    // Exercise both fixed-function and destination-reading GPU blend paths.
    packet.push(Command::SetBlendMode(if active_palette {
        BlendMode::Multiply
    } else {
        BlendMode::Screen
    }));
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(margin, third_y, cell * 1.7, cell * 0.82),
        radius: cell * 0.2,
        color: primary,
    });
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(
            margin + cell * 0.7,
            third_y + cell * 0.18,
            cell * 1.7,
            cell * 0.82,
        ),
        radius: cell * 0.2,
        color: accent,
    });
    packet.push(Command::Restore);
    packet.push(Command::EndLayer);

    if state.text_layout != 0 {
        packet.push(Command::DrawTextLayout {
            layout: state.text_layout,
            x: margin,
            y: third_y + cell * 1.18,
        });
    }

    // Pointer-following path proves pane-local motion input reaches the actor.
    let cursor = Path::new()
        .move_to(state.pointer_x, state.pointer_y - 14.0)
        .line_to(state.pointer_x + 14.0, state.pointer_y)
        .line_to(state.pointer_x, state.pointer_y + 14.0)
        .line_to(state.pointer_x - 14.0, state.pointer_y)
        .close();
    packet.push(Command::FillPath {
        path: cursor,
        color: Color::rgba(1.0, 1.0, 1.0, 0.92),
    });

    packet
}

fn star_path(center_x: f32, center_y: f32, radius: f32) -> Path {
    let mut points = [(0.0_f32, 0.0_f32); 10];
    for (index, point) in points.iter_mut().enumerate() {
        let point_radius = if index % 2 == 0 {
            radius
        } else {
            radius * 0.42
        };
        let angle = -core::f32::consts::FRAC_PI_2 + index as f32 * core::f32::consts::PI / 5.0;
        *point = (
            center_x + angle.cos() * point_radius,
            center_y + angle.sin() * point_radius,
        );
    }
    let mut path = Path::new().move_to(points[0].0, points[0].1);
    for point in &points[1..] {
        path = path.line_to(point.0, point.1);
    }
    path.close()
}

#[cfg(feature = "pure-renderer")]
fn decode_limits(len: usize) -> facet_codec::DecodeLimits {
    facet_codec::DecodeLimits {
        max_wire_bytes: len,
        max_allocated_bytes: len.saturating_mul(4).max(4096),
        max_allocations: 4096,
    }
}

#[cfg(feature = "pure-renderer")]
fn pointer_for(sequence: &SequenceState, pane: &facets::PaneTarget) -> (f32, f32) {
    sequence
        .pointers
        .iter()
        .find(|(id, _, _)| *id == pane.pane)
        .map(|(_, x, y)| (*x, *y))
        .unwrap_or((
            pane.logical_width as f32 * 0.5,
            pane.logical_height as f32 * 0.5,
        ))
}

#[cfg(feature = "pure-renderer")]
fn set_pointer(sequence: &mut SequenceState, pane: u64, x: f32, y: f32) {
    if let Some(entry) = sequence.pointers.iter_mut().find(|entry| entry.0 == pane) {
        *entry = (pane, x, y);
    } else {
        sequence.pointers.push((pane, x, y));
        sequence.pointers.sort_by_key(|entry| entry.0);
    }
}

#[cfg(feature = "pure-renderer")]
fn render_phase(view: RenderView, target_time_ns: u64) -> f32 {
    if view.animated {
        view.phase + target_time_ns as f32 * 1e-9 * core::f32::consts::TAU
    } else {
        view.phase
    }
}

#[cfg(feature = "pure-renderer")]
const fn next_animation_opportunity(animated: bool, target_time_ns: u64) -> Option<u64> {
    if animated {
        Some(target_time_ns.saturating_add(1))
    } else {
        None
    }
}

#[cfg(feature = "pure-renderer")]
impl Facets for GraphicsDemo {
    fn render(input: Vec<u8>) -> Vec<u8> {
        let request = facet_codec::decode_render_request(&input, decode_limits(input.len()))
            .expect("valid graphics render request");
        let view: RenderView = serde_json::from_slice(&request.application_view)
            .expect("valid immutable graphics view");
        let sequence: SequenceState =
            serde_json::from_slice(&request.sequence_state).unwrap_or_default();
        let phase = render_phase(view, request.target_time_ns);
        let panes = request.panes.iter().map(|pane| {
            let width = (pane.logical_width as f32).max(1.0);
            let height = (pane.logical_height as f32).max(1.0);
            let image = create_checker_image(pane.pane).expect("invocation-leased checker image");
            let mut frame = brood_plugin_sdk::graphics::UiFrame::new(pane.pane);
            let layout = frame.layout_text(
                "Canvas packets, native text — ffi العربية 日本語\nMove, click, scroll, or type to redraw",
                22.0,
                (width - 48.0).max(120.0),
                30.0,
                Color::rgba(0.9, 0.93, 0.98, 1.0),
            ).expect("invocation-leased demo title");
            let (pointer_x, pointer_y) = pointer_for(&sequence, pane);
            let packet = build_scene(ScenePane { width, height, image,
                text_layout: layout.handle, pointer_x, pointer_y }, phase, view.active_palette)
                .encode().expect("valid graphics packet");
            frame.finish_facet();
            facets::PaneOutput {
                pane: pane.pane,
                content: facets::PaneContent::Draw {
                    packet,
                        interaction: serde_json::to_vec(&InteractionMap {
                            pane: pane.pane,
                            phase,
                            animated: view.animated,
                        })
                        .expect("graphics interaction map"),
                },
            }
        }).collect();
        let response = facets::RenderResponse {
            token: request.token,
            panes,
            next_frame_ns: next_animation_opportunity(view.animated, request.target_time_ns),
        };
        facet_codec::encode_render_response_owned(
            &response,
            request.limits.max_output_bytes as usize,
        )
        .expect("bounded graphics response")
    }

    fn interpret(input: Vec<u8>) -> Vec<u8> {
        let request = facet_codec::decode_input_request(&input, decode_limits(input.len()))
            .expect("valid graphics input request");
        let map: InteractionMap =
            serde_json::from_slice(&request.interaction).expect("valid graphics interaction map");
        let mut sequence: SequenceState =
            serde_json::from_slice(&request.sequence_state).unwrap_or_default();
        let mut intents = Vec::new();
        let mut render = false;
        match &request.event {
            facets::InputEvent::PointerMotion { x, y } => {
                set_pointer(&mut sequence, map.pane, *x as f32, *y as f32);
                render = true;
            }
            facets::InputEvent::PointerButton {
                x,
                y,
                button,
                pressed,
                ..
            } if *button == LEFT_BUTTON => {
                set_pointer(&mut sequence, map.pane, *x as f32, *y as f32);
                if *pressed {
                    sequence.pressed_pane = Some(map.pane);
                    intents.push(FacetIntent::TogglePalette);
                } else if sequence.pressed_pane == Some(map.pane) {
                    sequence.pressed_pane = None;
                }
                render = true;
            }
            facets::InputEvent::PointerScroll { x, y, vertical, .. } => {
                set_pointer(&mut sequence, map.pane, *x as f32, *y as f32);
                intents.push(FacetIntent::AdvancePhase(*vertical as f32 * 0.08));
                render = true;
            }
            facets::InputEvent::Key {
                logical_key,
                pressed: true,
                repeat: false,
                ..
            } => {
                intents.push(if logical_key == "F8" {
                    FacetIntent::SetAnimation {
                        animated: !map.animated,
                        phase: map.phase,
                    }
                } else {
                    FacetIntent::AdvancePhase(0.35)
                });
                render = true;
            }
            facets::InputEvent::Text { .. }
            | facets::InputEvent::Paste { .. }
            | facets::InputEvent::Composition {
                committed: true, ..
            } => {
                intents.push(FacetIntent::TogglePalette);
                render = true;
            }
            facets::InputEvent::PointerLeave
            | facets::InputEvent::FocusLost
            | facets::InputEvent::CaptureLost => {
                sequence.pressed_pane = None;
            }
            _ => {}
        }
        let response = facets::InputResponse {
            token: request.token,
            next_sequence_state: serde_json::to_vec(&sequence).expect("graphics sequence state"),
            events: intents
                .into_iter()
                .map(|intent| serde_json::to_vec(&intent).expect("graphics intent"))
                .collect(),
            requests: if render {
                vec![facets::HostRequest::Render]
            } else {
                Vec::new()
            },
        };
        facet_codec::encode_input_response_owned(
            &response,
            request.limits.max_output_bytes as usize,
        )
        .expect("bounded graphics input response")
    }
}

#[cfg(all(test, feature = "pure-renderer"))]
mod tests {
    use super::*;

    fn scope() -> facets::Scope {
        facets::Scope {
            client_generation: 1,
            actor_generation: 2,
            facet_generation: 3,
        }
    }

    fn limits() -> facets::InvocationLimits {
        facets::InvocationLimits {
            max_output_bytes: 4096,
            max_packet_bytes: 4096,
            max_interaction_bytes: 1024,
            max_sequence_bytes: 1024,
            max_events: 8,
            max_event_bytes: 1024,
            max_host_requests: 2,
            max_resources: 16,
            max_resource_bytes: 128 * 1024,
        }
    }

    fn input_request(event: facets::InputEvent, sequence: SequenceState) -> Vec<u8> {
        let request = facets::InputRequest {
            token: facets::InputToken {
                scope: scope(),
                input_sequence: 4,
                frame: 5,
                pane: 9,
                snapshot_revision: 6,
                sequence_revision: 7,
            },
            geometry: facets::PaneTarget {
                pane: 9,
                logical_width: 320.0,
                logical_height: 200.0,
                physical_width: 640,
                physical_height: 400,
                scale: 2.0,
                clip: [0.0, 0.0, 320.0, 200.0],
                visible: true,
                focused: true,
            },
            event,
            interaction: serde_json::to_vec(&InteractionMap {
                pane: 9,
                phase: 1.25,
                animated: true,
            })
            .unwrap(),
            sequence_state: serde_json::to_vec(&sequence).unwrap(),
            limits: limits(),
        };
        let mut wire = vec![0; 4096];
        let len = facet_codec::encode_input_request(&request, &mut wire).unwrap();
        wire.truncate(len);
        wire
    }

    #[test]
    fn pointer_interpretation_is_paired_and_retry_deterministic() {
        let wire = input_request(
            facets::InputEvent::PointerMotion { x: 27.0, y: 41.0 },
            SequenceState::default(),
        );
        let first = GraphicsDemo::interpret(wire.clone());
        assert_eq!(first, GraphicsDemo::interpret(wire));
        let reply = facet_codec::decode_input_response(&first, decode_limits(first.len())).unwrap();
        let sequence: SequenceState = serde_json::from_slice(&reply.next_sequence_state).unwrap();
        assert_eq!(sequence.pointers, vec![(9, 27.0, 41.0)]);
        assert!(reply.events.is_empty());
        assert_eq!(reply.requests, vec![facets::HostRequest::Render]);
    }

    #[test]
    fn input_sequence_keeps_pane_local_pointer_state() {
        let mut sequence = SequenceState::default();
        set_pointer(&mut sequence, 11, 4.0, 5.0);
        set_pointer(&mut sequence, 9, 6.0, 7.0);
        set_pointer(&mut sequence, 11, 8.0, 9.0);
        assert_eq!(sequence.pointers, vec![(9, 6.0, 7.0), (11, 8.0, 9.0)]);
    }

    #[test]
    fn stopping_animation_commits_the_displayed_phase() {
        let wire = input_request(
            facets::InputEvent::Key {
                code: 66,
                logical_key: "F8".into(),
                modifiers: 0,
                pressed: true,
                repeat: false,
            },
            SequenceState::default(),
        );
        let bytes = GraphicsDemo::interpret(wire);
        let reply = facet_codec::decode_input_response(&bytes, decode_limits(bytes.len())).unwrap();
        assert_eq!(
            serde_json::from_slice::<FacetIntent>(&reply.events[0]).unwrap(),
            FacetIntent::SetAnimation {
                animated: false,
                phase: 1.25
            }
        );
    }

    #[test]
    fn scene_uses_exact_geometry_and_explicit_target_time() {
        let scene = ScenePane {
            width: 333.0,
            height: 217.0,
            image: 0,
            text_layout: 0,
            pointer_x: 17.0,
            pointer_y: 23.0,
        };
        let first = build_scene(
            scene,
            render_phase(
                RenderView {
                    phase: 0.25,
                    active_palette: false,
                    animated: true,
                },
                500_000_000,
            ),
            false,
        )
        .encode()
        .unwrap();
        let retry = build_scene(
            scene,
            render_phase(
                RenderView {
                    phase: 0.25,
                    active_palette: false,
                    animated: true,
                },
                500_000_000,
            ),
            false,
        )
        .encode()
        .unwrap();
        let other_geometry = build_scene(
            ScenePane {
                width: 334.0,
                ..scene
            },
            render_phase(
                RenderView {
                    phase: 0.25,
                    active_palette: false,
                    animated: true,
                },
                500_000_000,
            ),
            false,
        )
        .encode()
        .unwrap();
        let other_time = build_scene(
            scene,
            render_phase(
                RenderView {
                    phase: 0.25,
                    active_palette: false,
                    animated: true,
                },
                600_000_000,
            ),
            false,
        )
        .encode()
        .unwrap();
        assert_eq!(first, retry);
        assert_ne!(first, other_geometry);
        assert_ne!(first, other_time);
        assert_eq!(
            render_phase(
                RenderView {
                    phase: 0.25,
                    active_palette: false,
                    animated: false
                },
                u64::MAX
            ),
            0.25
        );
    }

    #[test]
    fn continuous_animation_requests_the_next_host_opportunity_at_any_refresh_rate() {
        for target_time_ns in [8_333_333, 16_666_667, 33_333_334] {
            assert_eq!(
                next_animation_opportunity(true, target_time_ns),
                Some(target_time_ns + 1)
            );
        }
        assert_eq!(next_animation_opportunity(false, 16_666_667), None);
        assert_eq!(next_animation_opportunity(true, u64::MAX), Some(u64::MAX));
    }
}

#[cfg(feature = "pure-renderer")]
export_visual_plugin!(GraphicsDemo);
#[cfg(not(feature = "pure-renderer"))]
export!(GraphicsDemo);
