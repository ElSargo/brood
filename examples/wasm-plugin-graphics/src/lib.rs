wit_bindgen::generate!({
    path: "../../crates/brood-wasm/wit",
    world: "brood-plugin",
});

use std::{sync::Mutex, time::Duration};

use brood::plugin::host;
use brood_graphics::{BlendMode, Color, Command, Packet, Path, Rect, Transform};
use exports::brood::plugin::actor::{Command as ActorCommand, Event, Guest, PresentationEvent};

const LEFT_BUTTON: u32 = 0x110;

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

struct DemoState {
    panes: Vec<DemoPane>,
    phase: f32,
    active_palette: bool,
    subscribed: bool,
}

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

static STATE: Mutex<DemoState> = Mutex::new(DemoState::new());

struct GraphicsDemo;

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

fn submit_scene(
    state: &DemoState,
    hint: &exports::brood::plugin::actor::RenderHint,
) -> Result<u64, String> {
    let draws = state
        .panes
        .iter()
        .map(|pane| {
            let packet = build_scene(*pane, state.phase, state.active_palette)
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

fn build_scene(state: DemoPane, phase: f32, active_palette: bool) -> Packet {
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

export!(GraphicsDemo);
