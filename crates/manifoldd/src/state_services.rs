use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::{
    session_bus::{SessionBusHandle, SessionBusRealm},
    state::SurfaceSessionId,
    wasm_test::{TestHarnessCleanupResult, TestHarnessCompletion, TestHarnessRuntime},
};

/// Owns the routing facts for surface sessions.
///
/// Compositor operations remain in `Manifold`; this service owns the catalogue
/// and makes catalogue transitions atomic and fallible before compositor state
/// is changed.
pub struct SurfaceSessionService {
    active: Option<SurfaceSessionId>,
    last_interacted: SurfaceSessionId,
    interaction_order: Vec<SurfaceSessionId>,
    next_id: u64,
    sessions: Vec<SurfaceSessionId>,
    names: BTreeMap<SurfaceSessionId, String>,
    working_directories: BTreeMap<SurfaceSessionId, PathBuf>,
    directory_sessions: BTreeMap<PathBuf, Vec<SurfaceSessionId>>,
    directory_defaults: BTreeMap<PathBuf, SurfaceSessionId>,
    surface_assignments: Vec<(WlSurface, SurfaceSessionId)>,
    buses: Vec<(SessionBusRealm, SessionBusHandle)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionServiceError {
    Unknown(SurfaceSessionId),
    AlreadyActive(SurfaceSessionId),
    NotInitialState,
    NoSessions,
    IdExhausted,
}

impl fmt::Display for SessionServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(session) => write!(f, "unknown surface session {}", session.raw()),
            Self::AlreadyActive(session) => {
                write!(f, "surface session {} is already active", session.raw())
            }
            Self::NotInitialState => f.write_str("surface session catalogue is not initial"),
            Self::NoSessions => f.write_str("surface session catalogue is empty"),
            Self::IdExhausted => f.write_str("surface session identifier space is exhausted"),
        }
    }
}

impl std::error::Error for SessionServiceError {}

impl SurfaceSessionService {
    pub(crate) fn new(initial_directory: PathBuf, initial_name: String) -> Self {
        let initial = SurfaceSessionId::from_raw(1);
        Self {
            active: Some(initial),
            last_interacted: initial,
            interaction_order: vec![initial],
            next_id: 2,
            sessions: vec![initial],
            names: std::iter::once((initial, initial_name)).collect(),
            working_directories: std::iter::once((initial, initial_directory)).collect(),
            directory_sessions: BTreeMap::new(),
            directory_defaults: BTreeMap::new(),
            surface_assignments: Vec::new(),
            buses: Vec::new(),
        }
    }

    pub(crate) const fn active(&self) -> SurfaceSessionId {
        match self.active_option() {
            Some(active) => active,
            None => SurfaceSessionId::NONE,
        }
    }

    pub(crate) const fn active_option(&self) -> Option<SurfaceSessionId> {
        self.active
    }

    pub(crate) fn sessions(&self) -> &[SurfaceSessionId] {
        &self.sessions
    }

    pub(crate) fn interaction_order(&self) -> &[SurfaceSessionId] {
        &self.interaction_order
    }

    pub(crate) const fn count(&self) -> usize {
        self.sessions.len()
    }

    pub(crate) const fn next_id(&self) -> SurfaceSessionId {
        SurfaceSessionId::from_raw(self.next_id)
    }

    pub(crate) fn contains(&self, session: SurfaceSessionId) -> bool {
        self.sessions.contains(&session)
    }

    pub(crate) fn clear_initial(&mut self) -> Result<(), SessionServiceError> {
        let initial = SurfaceSessionId::from_raw(1);
        if self.sessions != [initial] || !self.surface_assignments.is_empty() || self.next_id != 2 {
            return Err(SessionServiceError::NotInitialState);
        }
        self.active = None;
        self.last_interacted = SurfaceSessionId::NONE;
        self.interaction_order.clear();
        self.next_id = 1;
        self.sessions.clear();
        self.names.clear();
        self.working_directories.clear();
        self.directory_sessions.clear();
        self.directory_defaults.clear();
        Ok(())
    }

    pub(crate) fn create(
        &mut self,
        name: String,
        directory: PathBuf,
    ) -> Result<SurfaceSessionId, SessionServiceError> {
        let session = SurfaceSessionId::from_raw(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(SessionServiceError::IdExhausted)?;
        self.sessions.push(session);
        self.names.insert(session, name);
        self.working_directories.insert(session, directory);
        self.activate_created(session);
        Ok(session)
    }

    pub(crate) fn activate(
        &mut self,
        session: SurfaceSessionId,
    ) -> Result<(), SessionServiceError> {
        if !self.contains(session) {
            return Err(SessionServiceError::Unknown(session));
        }
        if self.active == Some(session) {
            return Err(SessionServiceError::AlreadyActive(session));
        }
        self.active = Some(session);
        self.record_interaction(session);
        Ok(())
    }

    pub(crate) fn activate_next(&mut self) -> Result<SurfaceSessionId, SessionServiceError> {
        if self.sessions.len() <= 1 {
            return Err(SessionServiceError::AlreadyActive(self.active()));
        }
        let next = self
            .sessions
            .iter()
            .position(|session| Some(*session) == self.active)
            .and_then(|index| self.sessions.get((index + 1) % self.sessions.len()))
            .copied()
            .or_else(|| self.sessions.first().copied())
            .ok_or(SessionServiceError::NoSessions)?;
        self.active = Some(next);
        self.record_interaction(next);
        Ok(next)
    }

    pub(crate) fn activate_recent(
        &mut self,
        session: SurfaceSessionId,
    ) -> Result<(), SessionServiceError> {
        self.activate(session)
    }

    fn activate_created(&mut self, session: SurfaceSessionId) {
        self.active = Some(session);
        self.record_interaction(session);
    }

    pub(crate) fn record_interaction(&mut self, session: SurfaceSessionId) -> bool {
        if !self.contains(session) {
            return false;
        }
        self.last_interacted = session;
        self.interaction_order
            .retain(|existing| *existing != session);
        self.interaction_order.insert(0, session);
        true
    }

    pub(crate) fn remove(
        &mut self,
        session: SurfaceSessionId,
    ) -> Result<SurfaceSessionId, SessionServiceError> {
        if !self.contains(session) {
            return Err(SessionServiceError::Unknown(session));
        }
        self.sessions.retain(|existing| *existing != session);
        self.names.remove(&session);
        self.working_directories.remove(&session);
        self.interaction_order
            .retain(|existing| *existing != session);
        self.directory_sessions.retain(|_, assigned| {
            assigned.retain(|candidate| *candidate != session);
            !assigned.is_empty()
        });
        self.directory_defaults
            .retain(|_, candidate| *candidate != session);
        self.surface_assignments
            .retain(|(_, candidate)| *candidate != session);
        self.buses
            .retain(|(realm, _)| *realm != SessionBusRealm::Surface(session.raw()));

        if self.active == Some(session) {
            self.active = self
                .interaction_order
                .iter()
                .copied()
                .find(|candidate| self.sessions.contains(candidate))
                .or_else(|| self.sessions.first().copied());
        }
        if let Some(active) = self.active
            && !self.interaction_order.contains(&active)
        {
            self.interaction_order.insert(0, active);
        }
        if self.last_interacted == session {
            self.last_interacted = self.active();
        }
        Ok(self.active())
    }

    pub(crate) fn rename(
        &mut self,
        session: SurfaceSessionId,
        name: &str,
    ) -> Result<(), SessionServiceError> {
        if !self.contains(session) {
            return Err(SessionServiceError::Unknown(session));
        }
        self.names.insert(session, name.to_string());
        Ok(())
    }

    pub(crate) fn name(&self, session: SurfaceSessionId) -> Option<&str> {
        self.names.get(&session).map(String::as_str)
    }

    pub(crate) fn working_directory(&self, session: SurfaceSessionId) -> Option<&Path> {
        self.working_directories.get(&session).map(PathBuf::as_path)
    }

    pub(crate) fn resolve(&self, query: &str) -> Option<SurfaceSessionId> {
        let query = query.trim();
        if query.is_empty() {
            return None;
        }
        if let Ok(raw) = query.parse::<u64>() {
            let session = SurfaceSessionId::from_raw(raw);
            if self.contains(session) {
                return Some(session);
            }
        }
        self.sessions.iter().copied().find(|session| {
            self.name(*session)
                .is_some_and(|name| name.eq_ignore_ascii_case(query))
        })
    }

    pub(crate) fn remember_directory(
        &mut self,
        directory: PathBuf,
        session: SurfaceSessionId,
        set_default: bool,
    ) -> Result<(), SessionServiceError> {
        if !self.contains(session) {
            return Err(SessionServiceError::Unknown(session));
        }
        let assigned = self
            .directory_sessions
            .entry(directory.clone())
            .or_default();
        if !assigned.contains(&session) {
            assigned.push(session);
        }
        self.working_directories
            .entry(session)
            .or_insert_with(|| directory.clone());
        if set_default
            || !self
                .directory_defaults
                .get(&directory)
                .is_some_and(|candidate| self.sessions.contains(candidate))
        {
            self.directory_defaults.insert(directory, session);
        }
        Ok(())
    }

    pub(crate) fn prune_directory(&mut self, directory: &Path) {
        if let Some(assigned) = self.directory_sessions.get_mut(directory) {
            assigned.retain(|session| self.sessions.contains(session));
            if assigned.is_empty() {
                self.directory_sessions.remove(directory);
            }
        }
        if self
            .directory_defaults
            .get(directory)
            .is_some_and(|session| !self.sessions.contains(session))
        {
            self.directory_defaults.remove(directory);
        }
    }

    pub(crate) fn directory_default(&self, directory: &Path) -> Option<SurfaceSessionId> {
        self.directory_defaults.get(directory).copied()
    }

    pub(crate) fn directory_sessions(&self, directory: &Path) -> &[SurfaceSessionId] {
        self.directory_sessions
            .get(directory)
            .map_or(&[], Vec::as_slice)
    }

    pub(crate) fn assign_surface(
        &mut self,
        surface: &WlSurface,
        session: SurfaceSessionId,
    ) -> Result<(), SessionServiceError> {
        if !self.contains(session) {
            return Err(SessionServiceError::Unknown(session));
        }
        self.surface_assignments
            .retain(|(assigned, _)| assigned != surface);
        self.surface_assignments.push((surface.clone(), session));
        Ok(())
    }

    pub(crate) fn surface_session(&self, surface: &WlSurface) -> Option<SurfaceSessionId> {
        self.surface_assignments
            .iter()
            .rev()
            .find_map(|(assigned, session)| (assigned == surface).then_some(*session))
    }

    pub(crate) fn surface_assignments(&self) -> &[(WlSurface, SurfaceSessionId)] {
        &self.surface_assignments
    }

    pub(crate) fn retain_live_surfaces(&mut self, live: &[WlSurface]) {
        self.surface_assignments
            .retain(|(surface, _)| live.iter().any(|candidate| candidate == surface));
    }

    pub(crate) fn buses(&self) -> &[(SessionBusRealm, SessionBusHandle)] {
        &self.buses
    }

    pub(crate) fn add_bus(&mut self, realm: SessionBusRealm, handle: SessionBusHandle) {
        self.buses.push((realm, handle));
    }
}

/// Owns focus identity independently from compositor focus application.
#[derive(Default)]
pub struct FocusService {
    active_toplevel: Option<WlSurface>,
    pointer_hover: Option<WlSurface>,
    pending_pointer_focus: Option<WlSurface>,
    presentation_window_focused: bool,
    pending_presentation_activation_token: Option<String>,
}

impl FocusService {
    pub(crate) const fn active_toplevel(&self) -> Option<&WlSurface> {
        self.active_toplevel.as_ref()
    }

    pub(crate) fn set_active_toplevel(&mut self, surface: Option<WlSurface>) {
        self.active_toplevel = surface;
    }

    pub(crate) const fn pointer_hover(&self) -> Option<&WlSurface> {
        self.pointer_hover.as_ref()
    }

    pub(crate) fn set_pointer_hover(&mut self, surface: Option<WlSurface>) {
        self.pointer_hover = surface;
    }

    pub(crate) fn set_pending_pointer_focus(&mut self, surface: Option<WlSurface>) {
        self.pending_pointer_focus = surface;
    }

    pub(crate) const fn pending_pointer_focus(&self) -> Option<&WlSurface> {
        self.pending_pointer_focus.as_ref()
    }

    pub(crate) const fn take_pending_pointer_focus(&mut self) -> Option<WlSurface> {
        self.pending_pointer_focus.take()
    }

    pub(crate) fn clear_transient_pointer_state(&mut self) {
        self.pending_pointer_focus = None;
        self.pointer_hover = None;
    }

    pub(crate) const fn presentation_window_focused(&self) -> bool {
        self.presentation_window_focused
    }

    pub(crate) const fn set_presentation_window_focused(&mut self, focused: bool) -> bool {
        let lost_focus = self.presentation_window_focused && !focused;
        self.presentation_window_focused = focused;
        lost_focus
    }

    pub(crate) fn queue_presentation_activation(&mut self, token: String) {
        self.pending_presentation_activation_token = Some(token);
    }

    pub(crate) fn clear_presentation_activation(&mut self) {
        self.pending_presentation_activation_token = None;
    }

    pub(crate) const fn take_presentation_activation(&mut self) -> Option<String> {
        self.pending_presentation_activation_token.take()
    }
}

/// Owns test-harness lifecycle state without owning compositor orchestration.
#[derive(Default)]
pub struct TestHarnessService {
    runtime: Option<TestHarnessRuntime>,
    completion: Option<TestHarnessCompletion>,
    cleanup: Option<TestHarnessCleanupResult>,
    nested_destination: Option<String>,
    nested_address: Option<String>,
}

impl TestHarnessService {
    pub(crate) fn install(&mut self, runtime: TestHarnessRuntime) -> Result<(), &'static str> {
        if self.runtime.is_some() {
            return Err("test harness is already installed");
        }
        if self.completion.is_some() {
            return Err("completed test harness state must be cleared before installation");
        }
        self.runtime = Some(runtime);
        Ok(())
    }

    pub(crate) const fn take_runtime(&mut self) -> Option<TestHarnessRuntime> {
        self.runtime.take()
    }

    pub(crate) fn restore_runtime(
        &mut self,
        runtime: TestHarnessRuntime,
    ) -> Result<(), &'static str> {
        if self.runtime.is_some() {
            return Err("test harness runtime ownership was already restored");
        }
        self.runtime = Some(runtime);
        Ok(())
    }

    pub(crate) const fn runtime(&self) -> Option<&TestHarnessRuntime> {
        self.runtime.as_ref()
    }

    pub(crate) fn record_completion(
        &mut self,
        completion: TestHarnessCompletion,
    ) -> Result<(), &'static str> {
        if self.completion.is_some() {
            return Err("test harness completion was already recorded");
        }
        self.completion = Some(completion);
        Ok(())
    }

    pub(crate) const fn completion(&self) -> Option<&TestHarnessCompletion> {
        self.completion.as_ref()
    }

    pub(crate) const fn is_complete(&self) -> bool {
        self.completion.is_some()
    }

    pub(crate) fn record_cleanup(
        &mut self,
        cleanup: TestHarnessCleanupResult,
    ) -> Result<(), &'static str> {
        if self.cleanup.is_some() {
            return Err("test harness cleanup was already recorded");
        }
        self.cleanup = Some(cleanup);
        Ok(())
    }

    pub(crate) const fn cleanup(&self) -> Option<&TestHarnessCleanupResult> {
        self.cleanup.as_ref()
    }

    pub(crate) fn configure_nested_target(
        &mut self,
        destination: Option<String>,
        address: Option<String>,
    ) -> Result<(), &'static str> {
        if address.is_some() && destination.is_none() {
            return Err("nested DBus address requires a destination");
        }
        self.nested_destination = destination;
        self.nested_address = address;
        Ok(())
    }

    pub(crate) fn nested_target(&self) -> Option<(&str, Option<&str>)> {
        self.nested_destination
            .as_deref()
            .map(|destination| (destination, self.nested_address.as_deref()))
    }
}
