//! Typed application commands for daemon-owned behavior.
//!
//! Transport adapters translate their wire representation into these requests.
//! This module owns operation-level policy; compositor state remains the adapter
//! that performs the underlying work.

use anyhow::{Context, Result, bail};

use crate::state::{Brood, PresentationWindowSummary, SurfaceSessionId, SurfaceSessionSummary};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommand {
    List,
    ListPresentationWindows,
    Active,
    Resolve {
        query: String,
    },
    Activate {
        session: SurfaceSessionId,
    },
    Create {
        name: Option<String>,
    },
    Rename {
        session: SurfaceSessionId,
        name: String,
    },
    Close {
        session: SurfaceSessionId,
    },
    SwitchNext,
    RestoreLastInteracted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommandResult {
    Sessions(Vec<SurfaceSessionSummary>),
    PresentationWindows(Vec<PresentationWindowSummary>),
    Active(SurfaceSessionId),
    Resolved(SurfaceSessionId),
    Transition(SessionTransition),
    Created(SurfaceSessionId),
    Renamed(bool),
    Closed(SessionClose),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTransition {
    pub active: SurfaceSessionId,
    pub changed: bool,
    /// Whether the requested target exists. Navigation requests always target
    /// the daemon-selected live session and therefore set this to true.
    pub target_exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionClose {
    pub closed: SurfaceSessionId,
    pub active: SurfaceSessionId,
    pub changed: bool,
}

impl SessionCommandResult {
    pub fn into_sessions(self) -> Result<Vec<SurfaceSessionSummary>> {
        match self {
            Self::Sessions(sessions) => Ok(sessions),
            result => bail!("session list request returned {result:?}"),
        }
    }

    pub fn into_presentation_windows(self) -> Result<Vec<PresentationWindowSummary>> {
        match self {
            Self::PresentationWindows(windows) => Ok(windows),
            result => bail!("presentation window list request returned {result:?}"),
        }
    }

    pub fn into_active(self) -> Result<SurfaceSessionId> {
        match self {
            Self::Active(session) => Ok(session),
            result => bail!("active session request returned {result:?}"),
        }
    }

    pub fn into_resolved(self) -> Result<SurfaceSessionId> {
        match self {
            Self::Resolved(session) => Ok(session),
            result => bail!("resolve session request returned {result:?}"),
        }
    }

    pub fn into_transition(self) -> Result<SessionTransition> {
        match self {
            Self::Transition(transition) => Ok(transition),
            result => bail!("session transition request returned {result:?}"),
        }
    }

    pub fn into_created(self) -> Result<SurfaceSessionId> {
        match self {
            Self::Created(session) => Ok(session),
            result => bail!("create session request returned {result:?}"),
        }
    }

    pub fn into_renamed(self) -> Result<bool> {
        match self {
            Self::Renamed(changed) => Ok(changed),
            result => bail!("rename session request returned {result:?}"),
        }
    }

    pub fn into_closed(self) -> Result<SessionClose> {
        match self {
            Self::Closed(closed) => Ok(closed),
            result => bail!("close session request returned {result:?}"),
        }
    }
}

pub struct SessionApplication<'state> {
    state: &'state mut Brood,
}

impl<'state> SessionApplication<'state> {
    pub const fn new(state: &'state mut Brood) -> Self {
        Self { state }
    }

    pub fn execute(&mut self, command: SessionCommand) -> Result<SessionCommandResult> {
        match command {
            SessionCommand::List => {
                self.state.refresh_window_state();
                Ok(SessionCommandResult::Sessions(
                    self.state.surface_session_summaries(),
                ))
            }
            SessionCommand::ListPresentationWindows => {
                self.state.refresh_window_state();
                Ok(SessionCommandResult::PresentationWindows(
                    self.state.presentation_window_summaries(),
                ))
            }
            SessionCommand::Active => Ok(SessionCommandResult::Active(
                self.state.active_surface_session(),
            )),
            SessionCommand::Resolve { query } => self
                .state
                .resolve_surface_session(&query)
                .map(SessionCommandResult::Resolved)
                .with_context(|| format!("unknown session {query:?}")),
            SessionCommand::Activate { session } => {
                let changed = self.state.switch_to_surface_session(session);
                let active = self.state.active_surface_session();
                Ok(SessionCommandResult::Transition(SessionTransition {
                    active,
                    changed,
                    target_exists: changed || active == session,
                }))
            }
            SessionCommand::Create { name } => {
                let session =
                    self.state
                        .create_and_attach_named_surface_session(normalize_session_name(
                            name.as_deref(),
                        ))?;
                Ok(SessionCommandResult::Created(session))
            }
            SessionCommand::Rename { session, name } => Ok(SessionCommandResult::Renamed(
                self.state.rename_surface_session(session, &name),
            )),
            SessionCommand::Close { session } => {
                let changed = self.state.close_surface_session(session);
                Ok(SessionCommandResult::Closed(SessionClose {
                    closed: session,
                    active: self.state.active_surface_session(),
                    changed,
                }))
            }
            SessionCommand::SwitchNext => {
                let changed = self.state.switch_to_next_surface_session();
                Ok(SessionCommandResult::Transition(SessionTransition {
                    active: self.state.active_surface_session(),
                    changed,
                    target_exists: true,
                }))
            }
            SessionCommand::RestoreLastInteracted => {
                let changed = self.state.restore_last_interacted_surface_session();
                Ok(SessionCommandResult::Transition(SessionTransition {
                    active: self.state.active_surface_session(),
                    changed,
                    target_exists: true,
                }))
            }
        }
    }
}

fn normalize_session_name(name: Option<&str>) -> Option<String> {
    let name = name?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::normalize_session_name;

    #[test]
    fn session_names_are_normalized_at_the_application_boundary() {
        assert_eq!(
            normalize_session_name(Some("  project\n  worktree  ")),
            Some("project worktree".to_string())
        );
        assert_eq!(normalize_session_name(Some(" \t ")), None);
        assert_eq!(normalize_session_name(None), None);
    }
}
