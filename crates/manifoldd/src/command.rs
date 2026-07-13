use tracing::warn;

use crate::{layout::LayoutAction, state::Manifold};

#[derive(Debug, Clone, PartialEq)]
pub enum WorkspaceCommand {
    Layout(LayoutAction),
    SpawnApp(Vec<String>),
    FocusPane(u64),
    FocusOrSpawnApp {
        app_id: String,
        argv: Vec<String>,
    },
    SetResolvedLayout {
        layout: crate::layout::ResolvedLayout,
        focus: Option<u64>,
    },
    Surface(SurfaceCommand),
    Input(InputCommand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceCommand {
    SwitchActiveSessionNext,
    CreateAndAttachSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputCommand {
    ToggleShortcutPassthrough,
    SetShortcutPassthrough(bool),
}

impl WorkspaceCommand {
    pub fn apply(self, state: &mut Manifold) -> bool {
        match self {
            Self::Layout(action) => state.apply_layout_action(action),
            Self::SpawnApp(argv) => match state.spawn_hosted_app_argv(&argv) {
                Ok(()) => true,
                Err(err) => {
                    warn!(?err, ?argv, "workspace command failed to spawn app");
                    false
                }
            },
            Self::FocusPane(pane) => state.focus_pane(pane),
            Self::FocusOrSpawnApp { app_id, argv } => state.focus_app_id_or_spawn(&app_id, &argv),
            Self::SetResolvedLayout { layout, focus } => {
                state.apply_plugin_resolved_layout(layout, focus)
            }
            Self::Surface(command) => command.apply(state),
            Self::Input(command) => command.apply(state),
        }
    }
}

impl SurfaceCommand {
    fn apply(self, state: &mut Manifold) -> bool {
        match self {
            Self::SwitchActiveSessionNext => state.switch_to_next_surface_session(),
            Self::CreateAndAttachSession => state.create_and_attach_surface_session().is_ok(),
        }
    }
}

impl InputCommand {
    fn apply(self, state: &mut Manifold) -> bool {
        match self {
            Self::ToggleShortcutPassthrough => {
                state.toggle_shortcut_passthrough();
                true
            }
            Self::SetShortcutPassthrough(enabled) => state.set_shortcut_passthrough(enabled),
        }
    }
}
