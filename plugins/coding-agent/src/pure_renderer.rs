use super::*;
use brood_plugin_sdk::{Facets, facet_codec, facets};

#[derive(Clone, Serialize, Deserialize)]
struct Preview {
    id: u64,
    name: String,
    width: u32,
    height: u32,
    #[serde(skip)]
    rgba: Vec<u8>,
}
#[derive(Clone, Serialize, Deserialize)]
struct View {
    agents: Vec<String>,
    views: BTreeMap<String, AgentView>,
    selected: Option<String>,
    prompt: String,
    prompt_cursor: usize,
    prompt_selection: Option<(usize, usize)>,
    scroll: f32,
    follow_tail: bool,
    search_active: bool,
    search_query: String,
    search_cursor: usize,
    search_match: usize,
    search_reveal: bool,
    images: Vec<Preview>,
    error: String,
}
impl View {
    fn from_ui(ui: &UiState) -> Self {
        Self {
            agents: ui.agents.clone(),
            views: ui
                .views
                .iter()
                .map(|(id, view)| {
                    (
                        id.clone(),
                        AgentView {
                            status: view.status.clone(),
                            model: view.model.clone(),
                            reasoning: view.reasoning.clone(),
                            logs: if ui.selected_agent() == Some(id.as_str()) {
                                view.logs.clone()
                            } else {
                                VecDeque::new()
                            },
                        },
                    )
                })
                .collect(),
            selected: ui.selected_agent().map(str::to_owned),
            prompt: ui.prompt.clone(),
            prompt_cursor: ui.prompt_cursor,
            prompt_selection: ui.prompt_selection,
            scroll: ui.transcript_scroll,
            follow_tail: ui.transcript_follow_tail,
            search_active: ui.search_active,
            search_query: ui.search_query.clone(),
            search_cursor: ui.search_cursor,
            search_match: ui.search_match,
            search_reveal: ui.search_reveal_pending,
            error: ui.error.clone(),
            images: ui
                .images
                .iter()
                .filter_map(|image| {
                    ui.image_previews.get(&image.id).map(|preview| Preview {
                        id: image.id,
                        name: image.name.clone(),
                        width: preview.width,
                        height: preview.height,
                        rgba: preview.rgba.clone(),
                    })
                })
                .collect(),
        }
    }
    fn render_state(&self, pane: &facets::PaneTarget) -> UiState {
        let mut ui = UiState::new();
        ui.pane = pane.pane;
        ui.width = pane.logical_width as f32;
        ui.height = pane.logical_height as f32;
        ui.pane_focused = pane.focused;
        ui.agents = self.agents.clone();
        ui.views = self.views.clone();
        ui.selected = self
            .selected
            .as_ref()
            .and_then(|id| ui.agents.iter().position(|item| item == id))
            .unwrap_or(0);
        ui.prompt = self.prompt.clone();
        ui.prompt_cursor = self.prompt_cursor;
        ui.prompt_selection = self.prompt_selection;
        ui.transcript_scroll = self.scroll;
        ui.transcript_follow_tail = self.follow_tail;
        ui.search_active = self.search_active;
        ui.search_query = self.search_query.clone();
        ui.search_cursor = self.search_cursor;
        ui.search_match = self.search_match;
        ui.search_reveal_pending = self.search_reveal;
        ui.error = self.error.clone();
        ui.images = self
            .images
            .iter()
            .map(|image| ImageAttachment {
                id: image.id,
                name: image.name.clone(),
                mime_type: String::new(),
                bytes: Vec::new(),
            })
            .collect();
        ui
    }
}
static PUBLICATION: Mutex<(u64, Vec<u8>)> = Mutex::new((0, Vec::new()));

fn encode_view(view: &View) -> Vec<u8> {
    let metadata = serde_json::to_vec(view).expect("coding-agent view metadata");
    let mut bytes = Vec::with_capacity(
        8 + metadata.len()
            + view
                .images
                .iter()
                .map(|image| image.rgba.len())
                .sum::<usize>(),
    );
    bytes.extend_from_slice(b"BAV1");
    bytes.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&metadata);
    for image in &view.images {
        bytes.extend_from_slice(&image.rgba);
    }
    bytes
}

fn decode_view(bytes: &[u8]) -> Result<View, &'static str> {
    if bytes.get(..4) != Some(b"BAV1".as_slice()) {
        return Err("invalid coding-agent view version");
    }
    let length = u32::from_le_bytes(
        bytes
            .get(4..8)
            .ok_or("truncated view header")?
            .try_into()
            .map_err(|_| "invalid view header")?,
    ) as usize;
    let metadata_end = 8usize.checked_add(length).ok_or("view metadata overflow")?;
    let mut view: View = serde_json::from_slice(
        bytes
            .get(8..metadata_end)
            .ok_or("truncated view metadata")?,
    )
    .map_err(|_| "invalid view metadata")?;
    if view.images.len() > MAX_IMAGES {
        return Err("too many view images");
    }
    let mut offset = metadata_end;
    for image in &mut view.images {
        let size = (image.width as usize)
            .checked_mul(image.height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or("view image size overflow")?;
        let end = offset
            .checked_add(size)
            .ok_or("view image offset overflow")?;
        image.rgba = bytes
            .get(offset..end)
            .ok_or("truncated view image")?
            .to_vec();
        offset = end;
    }
    if offset != bytes.len() {
        return Err("trailing view bytes");
    }
    Ok(view)
}

pub(super) fn publish() {
    let state = STATE.lock().expect("coding-agent state");
    let RoleState::Ui(ui) = &*state else {
        return;
    };
    if ui.pane == 0 {
        return;
    }
    // Pixel arrays in JSON exhausted the renderer's fuel with two attachments.
    // Keep metadata readable and carry preview pixels as bounded raw bytes.
    let bytes = encode_view(&View::from_ui(ui));
    drop(state);
    let mut last = PUBLICATION.lock().expect("coding-agent publication");
    if last.1 == bytes {
        return;
    }
    let revision = last
        .0
        .checked_add(1)
        .expect("coding-agent revision exhausted");
    if brood_plugin_sdk::publication::publish(revision, &bytes).is_err() {
        // Keep the last accepted root and retry the current view next turn.
        return;
    }
    *last = (revision, bytes);
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum Target {
    Agent(String),
    New,
    Search,
    Model,
    Reasoning,
    Send,
    Prompt,
}
fn resolve(ui: &UiState, target: &Target) -> Option<UiTarget> {
    Some(match target {
        Target::Agent(id) => UiTarget::AgentTab(ui.agents.iter().position(|item| item == id)?),
        Target::New => UiTarget::NewAgent,
        Target::Search => UiTarget::Search,
        Target::Model => UiTarget::Model,
        Target::Reasoning => UiTarget::Reasoning,
        Target::Send => UiTarget::Send,
        Target::Prompt => UiTarget::Prompt,
    })
}
#[derive(Serialize, Deserialize)]
struct Map {
    frame: u64,
    agent: Option<String>,
    agents: Vec<String>,
    regions: Vec<([f32; 4], Target)>,
    prompt: String,
    prompt_cursor: usize,
    prompt_rect: [f32; 4],
    width: f32,
    bar: Option<(f32, f32, f32, f32)>,
    scroll: f32,
    maximum: f32,
    viewport: f32,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Sequence {
    frame: u64,
    scroll: Option<f32>,
    agent: Option<String>,
    hovered: Option<Target>,
    pressed: Option<Target>,
    scrollbar_hovered: bool,
    drag_offset: Option<f32>,
    tab_drag: Option<(String, f32, bool)>,
}
#[derive(Debug, Serialize, Deserialize)]
enum Intent {
    Activate {
        agent: Option<String>,
        target: Target,
    },
    Cursor {
        agent: Option<String>,
        prompt: String,
        cursor: usize,
    },
    Scroll {
        agent: Option<String>,
        position: f32,
        maximum: f32,
    },
    Reorder {
        agent: String,
        before: Option<String>,
    },
    Key {
        agent: Option<String>,
        key: String,
        pressed: bool,
        modifiers: u32,
    },
    Text {
        agent: Option<String>,
        text: String,
        paste: bool,
    },
}
fn decode_limits(len: usize) -> facet_codec::DecodeLimits {
    facet_codec::DecodeLimits {
        max_wire_bytes: len,
        max_allocated_bytes: len.saturating_mul(4).max(4096),
        max_allocations: 16384,
    }
}
fn rect(rect: Rect) -> [f32; 4] {
    [rect.x, rect.y, rect.width, rect.height]
}
fn contains(rect: [f32; 4], x: f64, y: f64) -> bool {
    x.is_finite()
        && y.is_finite()
        && x >= f64::from(rect[0])
        && x < f64::from(rect[0] + rect[2])
        && y >= f64::from(rect[1])
        && y < f64::from(rect[1] + rect[3])
}
impl Facets for CodingAgentPlugin {
    fn render(input: Vec<u8>) -> Vec<u8> {
        let request = facet_codec::decode_render_request(&input, decode_limits(input.len()))
            .expect("coding-agent render request");
        let theme = ThemeSnapshot::decode(&request.configuration).expect("valid theme snapshot");
        let view = decode_view(&request.application_view).expect("coding-agent view");
        let sequence: Sequence =
            serde_json::from_slice(&request.sequence_state).unwrap_or_default();
        let panes = request
            .panes
            .iter()
            .map(|pane| {
                let mut ui = view.render_state(pane);
                if sequence.agent == view.selected {
                    ui.hovered = sequence
                        .hovered
                        .as_ref()
                        .and_then(|target| resolve(&ui, target));
                    ui.pressed = sequence
                        .pressed
                        .as_ref()
                        .and_then(|target| resolve(&ui, target));
                    ui.scrollbar_hovered =
                        sequence.scrollbar_hovered || sequence.drag_offset.is_some();
                }
                for image in &view.images {
                    let handle = host::graphics_create_image_rgba8(
                        pane.pane,
                        image.width,
                        image.height,
                        &image.rgba,
                    )
                    .expect("leased coding-agent preview");
                    ui.image_previews.insert(
                        image.id,
                        ImagePreview {
                            handle,
                            width: image.width,
                            height: image.height,
                            rgba: Vec::new(),
                        },
                    );
                }
                let mut cache = TextLayoutCache::new(pane.pane);
                cache.begin_frame();
                let packet = build_ui_packet(&theme, &mut ui, &mut cache);
                if let Some(error) = ui.draw_error.take() {
                    panic!("coding-agent rendering: {error}");
                }
                let mut regions = ui
                    .agents
                    .iter()
                    .enumerate()
                    .map(|(i, id)| (rect(agent_tab_rect(&ui, i)), Target::Agent(id.clone())))
                    .collect::<Vec<_>>();
                regions.push((rect(new_agent_rect(ui.width)), Target::New));
                regions.push((rect(search_button_rect(ui.width)), Target::Search));
                let (model, reasoning) = toolbar_control_rects(ui.width);
                if ui.selected_agent().is_some() {
                    regions.push((rect(model), Target::Model));
                    regions.push((rect(reasoning), Target::Reasoning));
                }
                if can_submit(&ui) {
                    regions.push((rect(send_button_rect(&ui)), Target::Send));
                }
                regions.push((rect(prompt_rect(&ui)), Target::Prompt));
                let map = Map {
                    frame: request.token.invocation,
                    agent: view.selected.clone(),
                    agents: view.agents.clone(),
                    regions,
                    prompt: view.prompt.clone(),
                    prompt_cursor: view.prompt_cursor,
                    prompt_rect: rect(prompt_rect(&ui)),
                    width: ui.width,
                    bar: scrollbar_geometry(&ui),
                    scroll: ui.transcript_scroll,
                    maximum: max_transcript_scroll(&ui),
                    viewport: ui.transcript_viewport_height,
                };
                facets::PaneOutput {
                    pane: pane.pane,
                    content: facets::PaneContent::Draw {
                        packet: packet.encode().expect("coding-agent packet"),
                        interaction: serde_json::to_vec(&map).expect("coding-agent interaction"),
                    },
                }
            })
            .collect();
        let response = facets::RenderResponse {
            token: request.token,
            panes,
            next_frame_ns: None,
        };
        facet_codec::encode_render_response_owned(
            &response,
            request.limits.max_output_bytes as usize,
        )
        .expect("bounded coding-agent render")
    }
    fn interpret(input: Vec<u8>) -> Vec<u8> {
        let request = facet_codec::decode_input_request(&input, decode_limits(input.len()))
            .expect("coding-agent input");
        let map: Map =
            serde_json::from_slice(&request.interaction).expect("coding-agent interaction");
        let mut sequence: Sequence =
            serde_json::from_slice(&request.sequence_state).unwrap_or_default();
        let (events, requests) = interpret_event(&map, &mut sequence, request.event);
        let response = facets::InputResponse {
            token: request.token,
            next_sequence_state: serde_json::to_vec(&sequence).expect("coding-agent sequence"),
            events,
            requests,
        };
        facet_codec::encode_input_response_owned(
            &response,
            request.limits.max_output_bytes as usize,
        )
        .expect("bounded coding-agent input")
    }
}

#[derive(Serialize, Deserialize)]
struct ImageMeta {
    agent: Option<String>,
    name: String,
    mime: String,
    width: u32,
    height: u32,
    source: usize,
    rgba: usize,
}
fn image_event(
    agent: Option<String>,
    name: String,
    mime: String,
    width: u32,
    height: u32,
    source: Vec<u8>,
    rgba: Vec<u8>,
) -> Vec<u8> {
    assert!(
        source.len() <= MAX_IMAGE_BYTES
            && width <= 192
            && height <= 80
            && rgba.len() == width as usize * height as usize * 4,
        "bounded image input"
    );
    let meta = serde_json::to_vec(&ImageMeta {
        agent,
        name,
        mime,
        width,
        height,
        source: source.len(),
        rgba: rgba.len(),
    })
    .expect("image metadata");
    assert!(meta.len() <= 16 * 1024, "bounded image metadata");
    let mut output = Vec::with_capacity(5 + meta.len() + source.len() + rgba.len());
    output.push(b'I');
    output.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    output.extend_from_slice(&meta);
    output.extend_from_slice(&source);
    output.extend_from_slice(&rgba);
    output
}
fn decode_image(bytes: &[u8]) -> Result<(ImageMeta, &[u8], &[u8]), String> {
    let len = bytes
        .get(1..5)
        .and_then(|part| part.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or("short image header")? as usize;
    if len > 16 * 1024 {
        return Err("image metadata too large".into());
    }
    let meta: ImageMeta =
        serde_json::from_slice(bytes.get(5..5 + len).ok_or("short image metadata")?)
            .map_err(|error| error.to_string())?;
    if meta.source > MAX_IMAGE_BYTES
        || meta.width == 0
        || meta.height == 0
        || meta.width > 192
        || meta.height > 80
        || meta.rgba != meta.width as usize * meta.height as usize * 4
    {
        return Err("invalid image dimensions or size".into());
    }
    let start = 5 + len;
    let end = start
        .checked_add(meta.source)
        .ok_or("image length overflow")?;
    if end.checked_add(meta.rgba) != Some(bytes.len()) {
        return Err("image payload length mismatch".into());
    }
    Ok((meta, &bytes[start..end], &bytes[end..]))
}
fn interpret_event(
    map: &Map,
    sequence: &mut Sequence,
    event: facets::InputEvent,
) -> (Vec<Vec<u8>>, Vec<facets::HostRequest>) {
    let mut requests = Vec::new();
    if sequence.agent != map.agent {
        if sequence.drag_offset.is_some() || sequence.tab_drag.is_some() {
            requests.push(facets::HostRequest::ReleaseCapture);
        }
        *sequence = Sequence {
            agent: map.agent.clone(),
            ..Sequence::default()
        };
    }
    if sequence.frame != map.frame {
        sequence.frame = map.frame;
        sequence.scroll = None;
    }
    if let facets::InputEvent::Image {
        name,
        mime_type,
        bytes,
        preview_width,
        preview_height,
        preview_rgba8,
    } = event
    {
        return (
            vec![image_event(
                map.agent.clone(),
                name,
                mime_type,
                preview_width,
                preview_height,
                bytes,
                preview_rgba8,
            )],
            requests,
        );
    }
    let mut events = Vec::new();
    let point = match &event {
        facets::InputEvent::PointerMotion { x, y }
        | facets::InputEvent::PointerButton { x, y, .. }
        | facets::InputEvent::PointerScroll { x, y, .. } => Some((*x, *y)),
        _ => None,
    };
    let target = point.and_then(|(x, y)| {
        map.regions
            .iter()
            .find(|(rect, _)| contains(*rect, x, y))
            .map(|(_, target)| target.clone())
    });
    let bar_hit = point.is_some_and(|(x, y)| {
        map.bar.is_some_and(|(top, height, _, _)| {
            contains(
                [
                    map.width - SCROLLBAR_HIT_WIDTH,
                    top,
                    SCROLLBAR_HIT_WIDTH,
                    height,
                ],
                x,
                y,
            )
        })
    });
    let scroll = |y: f64, offset: f32| {
        map.bar.map(|(top, height, _, thumb)| {
            let travel = (height - thumb).max(0.0);
            if travel > 0.0 {
                ((y as f32 - top - offset) / travel).clamp(0.0, 1.0) * map.maximum
            } else {
                0.0
            }
        })
    };
    match event {
        facets::InputEvent::PointerMotion { x, y } => {
            sequence.hovered = target.clone();
            sequence.scrollbar_hovered = bar_hit;
            if let Some(position) = sequence.drag_offset.and_then(|offset| scroll(y, offset)) {
                events.push(Intent::Scroll {
                    agent: map.agent.clone(),
                    position,
                    maximum: map.maximum,
                });
            } else if let Some((agent, start, moved)) = sequence.tab_drag.as_mut() {
                *moved |= (x as f32 - *start).abs() > 5.0;
                if *moved
                    && let Some(Target::Agent(before)) = target
                    && before != *agent
                {
                    let from = map.agents.iter().position(|id| id == agent).unwrap_or(0);
                    let to = map.agents.iter().position(|id| id == &before).unwrap_or(0);
                    let before = if to > from {
                        map.agents.get(to + 1).cloned()
                    } else {
                        Some(before)
                    };
                    events.push(Intent::Reorder {
                        agent: agent.clone(),
                        before,
                    });
                }
            }
        }
        facets::InputEvent::PointerButton {
            x,
            y,
            button,
            pressed,
            ..
        } if button == LEFT_BUTTON => {
            sequence.hovered = target.clone();
            sequence.scrollbar_hovered = bar_hit;
            if pressed {
                if bar_hit && let Some((_, _, thumb_y, thumb_height)) = map.bar {
                    let offset =
                        if y >= f64::from(thumb_y) && y <= f64::from(thumb_y + thumb_height) {
                            y as f32 - thumb_y
                        } else {
                            thumb_height * 0.5
                        };
                    sequence.drag_offset = Some(offset);
                    requests.push(facets::HostRequest::Capture);
                    if let Some(position) = scroll(y, offset) {
                        events.push(Intent::Scroll {
                            agent: map.agent.clone(),
                            position,
                            maximum: map.maximum,
                        });
                    }
                } else {
                    sequence.pressed = target.clone();
                    if let Some(Target::Agent(agent)) = target {
                        sequence.tab_drag = Some((agent, x as f32, false));
                        requests.push(facets::HostRequest::Capture);
                    }
                }
            } else {
                let dragged = sequence.tab_drag.take().is_some_and(|(_, _, moved)| moved);
                let scrollbar = sequence.drag_offset.take().is_some();
                requests.push(facets::HostRequest::ReleaseCapture);
                if !dragged
                    && !scrollbar
                    && sequence.pressed == target
                    && let Some(target) = target
                {
                    if target == Target::Prompt {
                        let mut ui = UiState::new();
                        ui.width = map.width;
                        ui.height = map.prompt_rect[1] + PROMPT_HEIGHT + 12.0;
                        ui.prompt = map.prompt.clone();
                        ui.prompt_cursor = map.prompt_cursor;
                        set_prompt_cursor_from_pointer(&mut ui, x as f32, y as f32);
                        events.push(Intent::Cursor {
                            agent: map.agent.clone(),
                            prompt: map.prompt.clone(),
                            cursor: ui.prompt_cursor,
                        });
                    } else {
                        events.push(Intent::Activate {
                            agent: map.agent.clone(),
                            target,
                        });
                    }
                }
                sequence.pressed = None;
            }
        }
        facets::InputEvent::PointerScroll { vertical, .. } => events.push(Intent::Scroll {
            agent: map.agent.clone(),
            position: {
                let position = (sequence.scroll.unwrap_or(map.scroll) - vertical as f32)
                    .clamp(0.0, map.maximum);
                sequence.scroll = Some(position);
                position
            },
            maximum: map.maximum,
        }),
        facets::InputEvent::Key {
            logical_key,
            pressed,
            modifiers,
            ..
        } => {
            if pressed && matches!(logical_key.as_str(), "PageUp" | "PageDown") {
                let delta = map.viewport * 0.82 * if logical_key == "PageUp" { -1.0 } else { 1.0 };
                events.push(Intent::Scroll {
                    agent: map.agent.clone(),
                    position: {
                        let position =
                            (sequence.scroll.unwrap_or(map.scroll) + delta).clamp(0.0, map.maximum);
                        sequence.scroll = Some(position);
                        position
                    },
                    maximum: map.maximum,
                });
            } else if !pressed && logical_key == "Tab" && !map.agents.is_empty() {
                let current = map
                    .agent
                    .as_ref()
                    .and_then(|id| map.agents.iter().position(|candidate| candidate == id))
                    .unwrap_or(0);
                events.push(Intent::Activate {
                    agent: map.agent.clone(),
                    target: Target::Agent(map.agents[(current + 1) % map.agents.len()].clone()),
                });
            } else {
                events.push(Intent::Key {
                    agent: map.agent.clone(),
                    key: logical_key,
                    pressed,
                    modifiers,
                });
            }
        }
        facets::InputEvent::Text { text }
        | facets::InputEvent::Composition {
            text,
            committed: true,
        } => events.push(Intent::Text {
            agent: map.agent.clone(),
            text,
            paste: false,
        }),
        facets::InputEvent::Paste { text } => events.push(Intent::Text {
            agent: map.agent.clone(),
            text,
            paste: true,
        }),
        facets::InputEvent::FocusLost
        | facets::InputEvent::CaptureLost
        | facets::InputEvent::PointerLeave => {
            requests.push(facets::HostRequest::ReleaseCapture);
            *sequence = Sequence {
                agent: map.agent.clone(),
                ..Sequence::default()
            };
        }
        _ => {}
    }
    requests.push(facets::HostRequest::Render);
    (
        events
            .into_iter()
            .map(|intent| serde_json::to_vec(&intent).expect("coding-agent intent"))
            .collect(),
        requests,
    )
}

pub(super) fn handle_input(events: Vec<Vec<u8>>) -> Vec<ActorCommand> {
    let mut guard = STATE.lock().expect("coding-agent state");
    let RoleState::Ui(ui) = &mut *guard else {
        return Vec::new();
    };
    let mut commands = Vec::new();
    for bytes in events {
        if bytes.first() == Some(&b'I') {
            match decode_image(&bytes) {
                Ok((meta, source, rgba)) if meta.agent.as_deref() == ui.selected_agent() => {
                    if let Err(error) = attach_ui_image(
                        ui,
                        meta.name,
                        meta.mime,
                        source.to_vec(),
                        meta.width,
                        meta.height,
                        rgba.to_vec(),
                    ) {
                        ui.error = error;
                    }
                }
                Ok(_) => {}
                Err(error) => ui.error = error,
            }
            continue;
        }
        let intent = match serde_json::from_slice::<Intent>(&bytes) {
            Ok(intent) => intent,
            Err(error) => {
                ui.error = format!("Invalid coding-agent input: {error}");
                continue;
            }
        };
        match intent {
            Intent::Activate { agent, target } => {
                if (matches!(target, Target::Agent(_) | Target::New)
                    || agent.as_deref() == ui.selected_agent())
                    && let Some(target) = resolve(ui, &target)
                {
                    commands.extend(activate_ui_target(ui, target));
                }
            }
            Intent::Cursor {
                agent,
                prompt,
                cursor,
            } if agent.as_deref() == ui.selected_agent()
                && prompt == ui.prompt
                && ui.prompt.is_char_boundary(cursor) =>
            {
                ui.search_active = false;
                set_prompt_cursor(ui, cursor, false);
            }
            Intent::Scroll {
                agent,
                position,
                maximum,
            } if agent.as_deref() == ui.selected_agent()
                && position.is_finite()
                && maximum.is_finite() =>
            {
                ui.transcript_scroll = position.clamp(0.0, maximum.max(0.0));
                ui.transcript_follow_tail = position >= maximum - 1.0;
                ui.search_reveal_pending = false;
            }
            Intent::Reorder { agent, before } => {
                if let Some(from) = ui.agents.iter().position(|id| id == &agent) {
                    let selected = ui.selected_agent().map(str::to_owned);
                    if before.as_ref().is_none_or(|id| ui.agents.contains(id)) {
                        let moved = ui.agents.remove(from);
                        let to = before
                            .as_ref()
                            .and_then(|id| ui.agents.iter().position(|item| item == id))
                            .unwrap_or(ui.agents.len());
                        ui.agents.insert(to, moved);
                        ui.selected = selected
                            .and_then(|id| ui.agents.iter().position(|item| item == &id))
                            .unwrap_or(0);
                    }
                }
            }
            Intent::Key {
                agent,
                key,
                pressed,
                modifiers,
            } if agent.as_deref() == ui.selected_agent() => {
                commands.extend(handle_ui_key(ui, &key, pressed, modifiers))
            }
            Intent::Text { agent, text, paste } if agent.as_deref() == ui.selected_agent() => {
                if !paste
                    && ui
                        .suppressed_text
                        .take()
                        .is_some_and(|key| key.eq_ignore_ascii_case(&text))
                {
                    continue;
                }
                insert_ui_text(ui, &text);
            }
            _ => {}
        }
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    fn map() -> Map {
        Map {
            frame: 1,
            agent: Some("agent-a".into()),
            agents: vec!["agent-a".into(), "agent-b".into()],
            regions: vec![
                ([0.0, 0.0, 80.0, 40.0], Target::Agent("agent-a".into())),
                ([80.0, 0.0, 80.0, 40.0], Target::Agent("agent-b".into())),
                ([200.0, 200.0, 80.0, 40.0], Target::Send),
            ],
            prompt: "hello".into(),
            prompt_cursor: 5,
            prompt_rect: [12.0, 200.0, 296.0, 96.0],
            width: 320.0,
            bar: None,
            scroll: 10.0,
            maximum: 100.0,
            viewport: 80.0,
        }
    }
    fn button(x: f64, y: f64, pressed: bool) -> facets::InputEvent {
        facets::InputEvent::PointerButton {
            x,
            y,
            button: LEFT_BUTTON,
            pressed,
            modifiers: 0,
        }
    }
    #[test]
    fn view_contains_preview_pixels_without_attachment_or_runtime_state() {
        let mut ui = UiState::new();
        ui.images.push(ImageAttachment {
            id: 9,
            name: "sample".into(),
            mime_type: "image/png".into(),
            bytes: vec![7; 1024 * 1024],
        });
        ui.image_previews.insert(
            9,
            ImagePreview {
                handle: 1234,
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 255],
            },
        );
        let bytes = encode_view(&View::from_ui(&ui));
        assert!(bytes.len() < 2048);
        let view = decode_view(&bytes).unwrap();
        assert_eq!(view.images[0].rgba, [1, 2, 3, 255]);
        assert!(decode_view(&bytes[..bytes.len() - 1]).is_err());
        let metadata_length = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let json: serde_json::Value =
            serde_json::from_slice(&bytes[8..8 + metadata_length]).unwrap();
        assert!(json["images"][0].get("rgba").is_none());
        assert!(json["images"][0].get("handle").is_none());
        assert!(json.get("notification").is_none());
        assert!(json.get("outbox").is_none());
    }
    #[test]
    fn attachment_encoding_is_bounded_raw_bytes_and_rejects_truncation() {
        let source = vec![255; MAX_IMAGE_BYTES];
        let rgba = vec![17; 192 * 80 * 4];
        let bytes = image_event(
            Some("agent-a".into()),
            "test.png".into(),
            "image/png".into(),
            192,
            80,
            source.clone(),
            rgba.clone(),
        );
        assert!(bytes.len() < 9 * 1024 * 1024);
        let (meta, decoded_source, decoded_rgba) = decode_image(&bytes).unwrap();
        assert_eq!(meta.agent.as_deref(), Some("agent-a"));
        assert_eq!(decoded_source, source);
        assert_eq!(decoded_rgba, rgba);
        assert!(decode_image(&bytes[..bytes.len() - 1]).is_err());
        let mut extra = bytes;
        extra.push(0);
        assert!(decode_image(&extra).is_err());
    }
    #[test]
    fn press_cannot_activate_replacement_agent_and_retry_is_identical() {
        let mut sequence = Sequence::default();
        interpret_event(&map(), &mut sequence, button(210.0, 210.0, true));
        let first = interpret_event(&map(), &mut sequence.clone(), button(210.0, 210.0, false));
        assert_eq!(
            first,
            interpret_event(&map(), &mut sequence.clone(), button(210.0, 210.0, false))
        );
        assert_eq!(first.0.len(), 1);
        let mut replacement = map();
        replacement.agent = Some("agent-b".into());
        replacement.frame = 2;
        assert!(
            interpret_event(&replacement, &mut sequence, button(210.0, 210.0, false))
                .0
                .is_empty()
        );
    }
    #[test]
    fn queued_scroll_accumulates_and_tab_returns_a_stable_identity() {
        let mut sequence = Sequence::default();
        let scroll = || facets::InputEvent::PointerScroll {
            x: 20.0,
            y: 80.0,
            horizontal: 0.0,
            vertical: -2.0,
            modifiers: 0,
        };
        interpret_event(&map(), &mut sequence, scroll());
        let events = interpret_event(&map(), &mut sequence, scroll()).0;
        let Intent::Scroll { position, .. } = serde_json::from_slice(&events[0]).unwrap() else {
            panic!("scroll");
        };
        assert_eq!(position, 14.0);
        let key = facets::InputEvent::Key {
            code: 0,
            logical_key: "Tab".into(),
            pressed: false,
            repeat: false,
            modifiers: 0,
        };
        let events = interpret_event(&map(), &mut sequence, key).0;
        let Intent::Activate { target, .. } = serde_json::from_slice(&events[0]).unwrap() else {
            panic!("activation");
        };
        assert_eq!(target, Target::Agent("agent-b".into()));
        let mut ui = UiState::new();
        ui.agents = vec!["agent-b".into(), "agent-a".into()];
        assert_eq!(resolve(&ui, &target), Some(UiTarget::AgentTab(0)));
        ui.agents.remove(0);
        assert_eq!(resolve(&ui, &target), None);
    }
}
