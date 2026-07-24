use tracing::warn;

use crate::state::Manifold;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceCommand {
    SpawnApp(Vec<String>),
    FocusPane(u64),
    FocusOrSpawnApp { app_id: String, argv: Vec<String> },
    Surface(SurfaceCommand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceCommand {
    SwitchActiveSessionNext,
    CreateAndAttachSession,
}

impl WorkspaceCommand {
    pub fn apply(self, state: &mut Manifold) -> bool {
        match self {
            Self::SpawnApp(argv) => match state.spawn_hosted_app_argv(&argv) {
                Ok(_) => true,
                Err(err) => {
                    warn!(?err, ?argv, "workspace command failed to spawn app");
                    false
                }
            },
            Self::FocusPane(pane) => state.focus_pane(pane),
            Self::FocusOrSpawnApp { app_id, argv } => state.focus_app_id_or_spawn(&app_id, &argv),
            Self::Surface(command) => command.apply(state),
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
