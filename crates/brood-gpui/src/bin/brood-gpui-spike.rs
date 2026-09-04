//! Independent GPUI lifecycle/conformance surface used by the nested harness.

use std::sync::mpsc;

use brood_gpui::{
    ActionSink, ManagedUiRoot, PresentationCommand, drive_presentation, presentation_channel,
};
use brood_protocol::managed_ui::{
    MANAGED_UI_PROTOCOL_VERSION, UiAccessibility, UiAxis, UiColorToken, UiDocument, UiNode,
    UiNodeKind, UiStateMode, UiStyle, UiTextRole,
};
use gpui::{App, AppContext as _, Application, Bounds, WindowBounds, WindowOptions, px, size};

fn main() {
    Application::new().run(|cx: &mut App| {
        let (action_sender, _action_receiver) = mpsc::sync_channel(64);
        let actions = ActionSink::new(action_sender);
        let document = conformance_document();
        let (presentation, commands) = presentation_channel(256);
        let bounds = Bounds::centered(None, size(px(960.0), px(640.0)), cx);
        let mut root = None;
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Brood GPUI".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| {
                let entity = cx.new(|cx| ManagedUiRoot::new(Vec::new(), actions.clone(), cx));
                root = Some(entity.clone());
                entity
            },
        );
        if let Err(error) = result {
            eprintln!("could not open GPUI presentation window: {error}");
            cx.quit();
            return;
        }
        let Some(root) = root else {
            eprintln!("GPUI did not construct the presentation root");
            cx.quit();
            return;
        };
        if presentation
            .try_send(PresentationCommand::Publish {
                actor_generation: 1,
                publication: brood_protocol::managed_ui::UiPublication::Snapshot(Box::new(
                    document,
                )),
            })
            .is_err()
        {
            eprintln!("could not enqueue the initial managed document");
            cx.quit();
            return;
        }
        cx.spawn(async move |cx| drive_presentation(commands, root, actions, cx.clone()))
            .detach();
        cx.activate(true);
    });
}

fn conformance_document() -> UiDocument {
    let node = |id, kind, children| UiNode {
        id,
        kind,
        style: UiStyle::default(),
        accessibility: UiAccessibility::default(),
        optional: false,
        interaction_revision: 0,
        children,
    };
    let text = |id, value: &str, role| UiNode {
        id,
        kind: UiNodeKind::Text {
            text: value.into(),
            role,
        },
        style: UiStyle::default(),
        accessibility: UiAccessibility::default(),
        optional: false,
        interaction_revision: 0,
        children: Vec::new(),
    };
    UiDocument {
        protocol_version: MANAGED_UI_PROTOCOL_VERSION,
        pane: 1,
        document_revision: 1,
        root: UiNode {
            id: 1,
            kind: UiNodeKind::Column,
            style: UiStyle {
                grow: 1.0,
                gap: 12.0,
                padding: brood_protocol::managed_ui::UiInsets {
                    top: 24.0,
                    right: 24.0,
                    bottom: 24.0,
                    left: 24.0,
                },
                background: Some(UiColorToken::Background),
                foreground: Some(UiColorToken::Foreground),
                ..UiStyle::default()
            },
            accessibility: UiAccessibility {
                label: Some("Brood presentation".into()),
                ..UiAccessibility::default()
            },
            optional: false,
            interaction_revision: 0,
            children: vec![
                text(2, "Brood", UiTextRole::Heading),
                text(
                    3,
                    "GPUI owns this application and window lifecycle.",
                    UiTextRole::Body,
                ),
                UiNode {
                    id: 4,
                    kind: UiNodeKind::Button {
                        label: "Semantic action".into(),
                    },
                    style: UiStyle {
                        width: Some(180.0),
                        padding: brood_protocol::managed_ui::UiInsets {
                            top: 8.0,
                            right: 12.0,
                            bottom: 8.0,
                            left: 12.0,
                        },
                        background: Some(UiColorToken::Accent),
                        radius: 6.0,
                        ..UiStyle::default()
                    },
                    accessibility: UiAccessibility {
                        label: Some("Emit semantic action".into()),
                        ..UiAccessibility::default()
                    },
                    optional: false,
                    interaction_revision: 0,
                    children: Vec::new(),
                },
                node(
                    5,
                    UiNodeKind::ScrollRegion {
                        axis: UiAxis::Vertical,
                    },
                    vec![node(
                        6,
                        UiNodeKind::Grid { columns: 3 },
                        vec![
                            node(7, UiNodeKind::Row, vec![text(8, "Row", UiTextRole::Label)]),
                            node(
                                9,
                                UiNodeKind::Stack,
                                vec![text(10, "Stack", UiTextRole::Caption)],
                            ),
                            node(11, UiNodeKind::Spacer, Vec::new()),
                            node(
                                12,
                                UiNodeKind::Divider {
                                    axis: UiAxis::Horizontal,
                                },
                                Vec::new(),
                            ),
                            node(
                                13,
                                UiNodeKind::VirtualList {
                                    item_count: 10_000,
                                    estimated_item_extent: 24.0,
                                },
                                vec![text(14, "Virtual row", UiTextRole::Body)],
                            ),
                            node(
                                15,
                                UiNodeKind::Icon {
                                    name: "search".into(),
                                },
                                Vec::new(),
                            ),
                            node(
                                16,
                                UiNodeKind::Image {
                                    resource: 0,
                                    alt: "Unavailable conformance image".into(),
                                },
                                Vec::new(),
                            ),
                            node(
                                17,
                                UiNodeKind::Badge {
                                    text: "Badge".into(),
                                },
                                Vec::new(),
                            ),
                            node(18, UiNodeKind::Progress { value: Some(0.62) }, Vec::new()),
                            node(
                                19,
                                UiNodeKind::Separator {
                                    axis: UiAxis::Horizontal,
                                },
                                Vec::new(),
                            ),
                            node(
                                20,
                                UiNodeKind::Toggle {
                                    label: "Hybrid toggle".into(),
                                    value: true,
                                    state_mode: UiStateMode::Hybrid,
                                },
                                Vec::new(),
                            ),
                            node(
                                21,
                                UiNodeKind::RadioGroup {
                                    value: Some("first".into()),
                                    state_mode: UiStateMode::Controlled,
                                },
                                vec![
                                    node(
                                        22,
                                        UiNodeKind::SelectableRow { selected: true },
                                        vec![text(23, "First", UiTextRole::Body)],
                                    ),
                                    node(
                                        24,
                                        UiNodeKind::SelectableRow { selected: false },
                                        vec![text(25, "Second", UiTextRole::Body)],
                                    ),
                                ],
                            ),
                            node(
                                26,
                                UiNodeKind::Menu {
                                    label: "Menu".into(),
                                },
                                Vec::new(),
                            ),
                            node(
                                27,
                                UiNodeKind::Link {
                                    label: "Link".into(),
                                    target: "brood://conformance".into(),
                                },
                                Vec::new(),
                            ),
                            node(
                                28,
                                UiNodeKind::CommandTarget {
                                    command: "conformance.command".into(),
                                },
                                vec![text(29, "Command target", UiTextRole::Label)],
                            ),
                            node(
                                30,
                                UiNodeKind::TextField {
                                    value: "Text".into(),
                                    placeholder: "Type".into(),
                                    state_mode: UiStateMode::Hybrid,
                                },
                                Vec::new(),
                            ),
                            node(
                                31,
                                UiNodeKind::MultilineEditor {
                                    value: "Multiline\neditor".into(),
                                    state_mode: UiStateMode::HostTransient,
                                },
                                Vec::new(),
                            ),
                            node(
                                32,
                                UiNodeKind::SearchField {
                                    value: String::new(),
                                    placeholder: "Search".into(),
                                    state_mode: UiStateMode::Controlled,
                                },
                                Vec::new(),
                            ),
                            node(33, UiNodeKind::TextEditorBridge { editor: 1 }, Vec::new()),
                            node(
                                34,
                                UiNodeKind::Tooltip {
                                    text: "Tooltip".into(),
                                },
                                vec![text(35, "Hover target", UiTextRole::Caption)],
                            ),
                            node(
                                36,
                                UiNodeKind::Popover,
                                vec![text(37, "Popover", UiTextRole::Body)],
                            ),
                            node(
                                38,
                                UiNodeKind::Dialog {
                                    title: "Dialog".into(),
                                },
                                vec![text(39, "Dialog content", UiTextRole::Body)],
                            ),
                            node(
                                40,
                                UiNodeKind::ContextMenu,
                                vec![text(41, "Context action", UiTextRole::Body)],
                            ),
                            node(
                                42,
                                UiNodeKind::DragSource {
                                    payload_type: "text/plain".into(),
                                },
                                vec![text(43, "Drag", UiTextRole::Label)],
                            ),
                            node(
                                44,
                                UiNodeKind::DropTarget {
                                    accepted_types: vec!["text/plain".into()],
                                },
                                vec![text(45, "Drop", UiTextRole::Label)],
                            ),
                            node(46, UiNodeKind::Canvas { target: 1 }, Vec::new()),
                            node(47, UiNodeKind::ExternalSurface { surface: 1 }, Vec::new()),
                            node(48, UiNodeKind::TerminalView { terminal: 1 }, Vec::new()),
                        ],
                    )],
                ),
            ],
        },
    }
}
