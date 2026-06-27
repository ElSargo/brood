use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, KeyState},
    input::pointer::AxisFrame,
    reexports::calloop::{
        EventLoop,
        channel::{self, Event},
    },
    utils::Point,
};
use tracing::{info, warn};
use zbus::{fdo, message::Header, proxy};

use crate::{
    CtlArgs, DEFAULT_MANIFOLD_DBUS_NAME, EnterArgs, HideArgs, ListArgs, MANIFOLD_SESSION_ENV,
    MANIFOLDD_DBUS_ADDRESS_ENV, MANIFOLDD_DBUS_DESTINATION_ENV, ShowArgs, SwitchArgs, browser,
    command::{SurfaceCommand, WorkspaceCommand},
    debug_dump::{self, DumpOptions},
    layout::LayoutAction,
    state::{
        FocusDirection, Manifold, PresentationStatus, SurfaceSessionId, SurfaceSessionSummary,
    },
};

const CONTROL_OBJECT_PATH: &str = "/org/manifold/Session";
const MANAGER_OBJECT_PATH: &str = "/org/manifold/Manager";
const EXTENSION_OBJECT_PATH: &str = "/org/manifold/Extension";
const COMPOSITOR_OBJECT_PATH: &str = "/org/manifold/Compositor";
const CONTROL_INTERFACE: &str = "org.manifold.Session.Control";
const MANAGER_INTERFACE: &str = "org.manifold.Manager";
const EXTENSION_INTERFACE: &str = "org.manifold.Extension";
const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(15);

type DbusMetadata = BTreeMap<String, String>;
type DbusSurfaceSession = (u64, String, bool, u64);
type DbusExtension = (String, String, String, DbusMetadata);
type DbusGrant = (String, String, String, String, bool, DbusMetadata);

#[derive(Clone, Debug)]
pub(crate) struct ControlTarget {
    pub(crate) destination: String,
    pub(crate) address: Option<String>,
}

impl ControlTarget {
    pub(crate) fn new(destination: String, address: Option<String>) -> Self {
        Self {
            destination,
            address,
        }
    }

    pub(crate) fn session_marker(&self) -> String {
        self.session_marker_with_session(None)
    }

    pub(crate) fn session_marker_with_session(&self, surface_session_id: Option<u64>) -> String {
        let mut marker = format!("dbus-destination={}", self.destination);
        if let Some(address) = self.address.as_deref() {
            marker.push_str(" dbus-address=");
            marker.push_str(address);
        }
        if let Some(session_id) = surface_session_id {
            marker.push_str(" surface-session-id=");
            marker.push_str(&session_id.to_string());
        }
        marker
    }
}

type ControlResponse = mpsc::Sender<std::result::Result<String, String>>;
type BoolResponse = mpsc::Sender<std::result::Result<bool, String>>;
type U64Response = mpsc::Sender<std::result::Result<u64, String>>;
type SessionListResponse = mpsc::Sender<std::result::Result<Vec<DbusSurfaceSession>, String>>;
type ExtensionListResponse = mpsc::Sender<std::result::Result<Vec<DbusExtension>, String>>;
type GrantListResponse = mpsc::Sender<std::result::Result<Vec<DbusGrant>, String>>;

enum DbusStateRequest {
    ControlCommand {
        command: Vec<String>,
        json: bool,
        response: ControlResponse,
    },
    CurrentStatus {
        response: ControlResponse,
    },
    ListSurfaceSessions {
        response: SessionListResponse,
    },
    ActiveSession {
        response: U64Response,
    },
    ResolveSession {
        query: String,
        response: U64Response,
    },
    ActivateSurfaceSession {
        id: u64,
        response: BoolResponse,
    },
    CreateSurfaceSession {
        name: String,
        response: U64Response,
    },
    RenameSurfaceSession {
        id: u64,
        name: String,
        response: BoolResponse,
    },
    CloseSurfaceSession {
        id: u64,
        response: BoolResponse,
    },
    ListExtensions {
        response: ExtensionListResponse,
    },
    RegisterExtension {
        role: String,
        metadata: DbusMetadata,
        sender_name: String,
        response: ControlResponse,
    },
    UnregisterExtension {
        extension_id: String,
        response: BoolResponse,
    },
    ListGrants {
        extension_id: String,
        response: GrantListResponse,
    },
    ReleaseGrant {
        grant_id: String,
        response: BoolResponse,
    },
    RegisterWindow {
        extension_id: String,
        window_identity: DbusMetadata,
        role: String,
        sender_name: String,
        response: ControlResponse,
    },
    RequestPlacement {
        window_id: String,
        placement: DbusMetadata,
        sender_name: String,
        response: ControlResponse,
    },
    RequestPresentationMembership {
        window_id: String,
        membership: DbusMetadata,
        sender_name: String,
        response: ControlResponse,
    },
    RequestShortcut {
        extension_id: String,
        shortcut: String,
        scope: String,
        label: String,
        sender_name: String,
        response: ControlResponse,
    },
    RequestFocus {
        window_id: String,
        response: BoolResponse,
    },
    Dismiss {
        window_id: String,
        response: BoolResponse,
    },
}

#[derive(Clone)]
struct ControlDbus {
    sender: channel::Sender<DbusStateRequest>,
}

#[derive(Clone)]
struct ManagerDbus {
    sender: channel::Sender<DbusStateRequest>,
}

#[derive(Clone)]
struct SessionDbus {
    sender: channel::Sender<DbusStateRequest>,
}

#[derive(Clone)]
struct ExtensionDbus {
    sender: channel::Sender<DbusStateRequest>,
}

#[derive(Clone)]
struct CompositorDbus {
    sender: channel::Sender<DbusStateRequest>,
}

#[zbus::interface(name = "org.manifold.Session.Control")]
impl ControlDbus {
    async fn command(&self, argv: Vec<String>, json: bool) -> fdo::Result<String> {
        request_state(&self.sender, |response| DbusStateRequest::ControlCommand {
            command: argv,
            json,
            response,
        })
    }
}

#[zbus::interface(name = "org.manifold.Manager")]
impl ManagerDbus {
    async fn list_sessions(&self) -> fdo::Result<Vec<DbusSurfaceSession>> {
        request_state(&self.sender, |response| {
            DbusStateRequest::ListSurfaceSessions { response }
        })
    }

    async fn active_session(&self) -> fdo::Result<u64> {
        request_state(&self.sender, |response| DbusStateRequest::ActiveSession {
            response,
        })
    }

    async fn resolve_session(&self, session: String) -> fdo::Result<u64> {
        request_state(&self.sender, |response| DbusStateRequest::ResolveSession {
            query: session,
            response,
        })
    }

    async fn list_extensions(&self) -> fdo::Result<Vec<DbusExtension>> {
        request_state(&self.sender, |response| DbusStateRequest::ListExtensions {
            response,
        })
    }
}

#[zbus::interface(name = "org.manifold.Session")]
impl SessionDbus {
    async fn current_status(&self) -> fdo::Result<String> {
        request_state(&self.sender, |response| DbusStateRequest::CurrentStatus {
            response,
        })
    }

    async fn list_surface_sessions(&self) -> fdo::Result<Vec<DbusSurfaceSession>> {
        request_state(&self.sender, |response| {
            DbusStateRequest::ListSurfaceSessions { response }
        })
    }

    async fn activate_surface_session(&self, id: u64) -> fdo::Result<bool> {
        request_state(&self.sender, |response| {
            DbusStateRequest::ActivateSurfaceSession { id, response }
        })
    }

    async fn create_surface_session(&self, name: String) -> fdo::Result<u64> {
        request_state(&self.sender, |response| {
            DbusStateRequest::CreateSurfaceSession { name, response }
        })
    }

    async fn rename_surface_session(&self, id: u64, name: String) -> fdo::Result<bool> {
        request_state(&self.sender, |response| {
            DbusStateRequest::RenameSurfaceSession { id, name, response }
        })
    }

    async fn close_surface_session(&self, id: u64) -> fdo::Result<bool> {
        request_state(&self.sender, |response| {
            DbusStateRequest::CloseSurfaceSession { id, response }
        })
    }
}

#[zbus::interface(name = "org.manifold.Extension")]
impl ExtensionDbus {
    async fn register_extension(
        &self,
        role: String,
        metadata: DbusMetadata,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<String> {
        let sender_name = header_sender(&header);
        request_state(&self.sender, |response| {
            DbusStateRequest::RegisterExtension {
                role,
                metadata,
                sender_name,
                response,
            }
        })
    }

    async fn unregister_extension(&self, extension_id: String) -> fdo::Result<bool> {
        request_state(&self.sender, |response| {
            DbusStateRequest::UnregisterExtension {
                extension_id,
                response,
            }
        })
    }

    async fn list_grants(&self, extension_id: String) -> fdo::Result<Vec<DbusGrant>> {
        request_state(&self.sender, |response| DbusStateRequest::ListGrants {
            extension_id,
            response,
        })
    }

    async fn release_grant(&self, grant_id: String) -> fdo::Result<bool> {
        request_state(&self.sender, |response| DbusStateRequest::ReleaseGrant {
            grant_id,
            response,
        })
    }
}

#[zbus::interface(name = "org.manifold.Compositor")]
impl CompositorDbus {
    async fn register_window(
        &self,
        extension_id: String,
        window_identity: DbusMetadata,
        role: String,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<String> {
        let sender_name = header_sender(&header);
        request_state(&self.sender, |response| DbusStateRequest::RegisterWindow {
            extension_id,
            window_identity,
            role,
            sender_name,
            response,
        })
    }

    async fn request_placement(
        &self,
        window_id: String,
        placement: DbusMetadata,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<String> {
        let sender_name = header_sender(&header);
        request_state(&self.sender, |response| {
            DbusStateRequest::RequestPlacement {
                window_id,
                placement,
                sender_name,
                response,
            }
        })
    }

    async fn request_presentation_membership(
        &self,
        window_id: String,
        membership: DbusMetadata,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<String> {
        let sender_name = header_sender(&header);
        request_state(&self.sender, |response| {
            DbusStateRequest::RequestPresentationMembership {
                window_id,
                membership,
                sender_name,
                response,
            }
        })
    }

    async fn request_shortcut(
        &self,
        extension_id: String,
        shortcut: String,
        scope: String,
        label: String,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<String> {
        let sender_name = header_sender(&header);
        request_state(&self.sender, |response| DbusStateRequest::RequestShortcut {
            extension_id,
            shortcut,
            scope,
            label,
            sender_name,
            response,
        })
    }

    async fn request_focus(&self, window_id: String) -> fdo::Result<bool> {
        request_state(&self.sender, |response| DbusStateRequest::RequestFocus {
            window_id,
            response,
        })
    }

    async fn dismiss(&self, window_id: String) -> fdo::Result<bool> {
        request_state(&self.sender, |response| DbusStateRequest::Dismiss {
            window_id,
            response,
        })
    }
}

fn request_state<T>(
    sender: &channel::Sender<DbusStateRequest>,
    build: impl FnOnce(mpsc::Sender<std::result::Result<T, String>>) -> DbusStateRequest,
) -> fdo::Result<T> {
    let (response_tx, response_rx) = mpsc::channel();
    sender
        .send(build(response_tx))
        .map_err(|_| fdo::Error::Failed("Manifold session loop is not running".to_string()))?;
    response_rx
        .recv_timeout(CONTROL_REPLY_TIMEOUT)
        .map_err(|_| {
            fdo::Error::Failed("timed out waiting for Manifold control response".to_string())
        })?
        .map_err(fdo::Error::Failed)
}

fn header_sender(header: &Header<'_>) -> String {
    header
        .sender()
        .map(ToString::to_string)
        .unwrap_or_else(|| "unknown".to_string())
}

#[proxy(
    interface = "org.manifold.Session.Control",
    default_path = "/org/manifold/Session"
)]
trait ManifoldControl {
    fn command(&self, argv: Vec<String>, json: bool) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.manifold.Manager",
    default_path = "/org/manifold/Manager"
)]
trait ManifoldManager {
    fn list_sessions(&self) -> zbus::Result<Vec<DbusSurfaceSession>>;
    fn active_session(&self) -> zbus::Result<u64>;
    fn resolve_session(&self, session: String) -> zbus::Result<u64>;
    fn list_extensions(&self) -> zbus::Result<Vec<DbusExtension>>;
}

#[proxy(
    interface = "org.manifold.Session",
    default_path = "/org/manifold/Session"
)]
trait ManifoldSession {
    fn current_status(&self) -> zbus::Result<String>;
    fn list_surface_sessions(&self) -> zbus::Result<Vec<DbusSurfaceSession>>;
    fn activate_surface_session(&self, id: u64) -> zbus::Result<bool>;
    fn create_surface_session(&self, name: String) -> zbus::Result<u64>;
    fn rename_surface_session(&self, id: u64, name: String) -> zbus::Result<bool>;
    fn close_surface_session(&self, id: u64) -> zbus::Result<bool>;
}

#[proxy(
    interface = "org.manifold.Extension",
    default_path = "/org/manifold/Extension"
)]
trait ManifoldExtension {
    fn register_extension(&self, role: String, metadata: DbusMetadata) -> zbus::Result<String>;
    fn unregister_extension(&self, extension_id: String) -> zbus::Result<bool>;
    fn list_grants(&self, extension_id: String) -> zbus::Result<Vec<DbusGrant>>;
    fn release_grant(&self, grant_id: String) -> zbus::Result<bool>;
}

#[proxy(
    interface = "org.manifold.Compositor",
    default_path = "/org/manifold/Compositor"
)]
trait ManifoldCompositor {
    fn register_window(
        &self,
        extension_id: String,
        window_identity: DbusMetadata,
        role: String,
    ) -> zbus::Result<String>;
    fn request_placement(&self, window_id: String, placement: DbusMetadata)
    -> zbus::Result<String>;
    fn request_presentation_membership(
        &self,
        window_id: String,
        membership: DbusMetadata,
    ) -> zbus::Result<String>;
    fn request_shortcut(
        &self,
        extension_id: String,
        shortcut: String,
        scope: String,
        label: String,
    ) -> zbus::Result<String>;
    fn request_focus(&self, window_id: String) -> zbus::Result<bool>;
    fn dismiss(&self, window_id: String) -> zbus::Result<bool>;
}

pub fn listen(event_loop: &mut EventLoop<'static, Manifold>, target: ControlTarget) -> Result<()> {
    let (sender, receiver) = channel::channel::<DbusStateRequest>();
    event_loop
        .handle()
        .insert_source(receiver, |event, _, state| {
            let Event::Msg(request) = event else {
                return;
            };
            execute_state_request(request, state);
        })
        .map_err(|err| anyhow::anyhow!("insert DBus control request channel: {err:?}"))?;

    start_dbus_service(target.clone(), sender)?;

    info!(
        dbus_destination = %target.destination,
        dbus_address = ?target.address,
        "DBus control service listening"
    );
    Ok(())
}

pub fn send(args: CtlArgs) -> Result<()> {
    let target = resolve_control_target(args.dbus_name, args.dbus_address);
    if !args.json && handle_typed_ctl_command(&target, &args.command)? {
        return Ok(());
    }
    let response = request_dbus_command(&target, &args.command, args.json)?;
    print!("{response}");
    Ok(())
}

pub fn enter(args: EnterArgs) -> Result<()> {
    let already_entered = entered_session_marker().is_some();
    if already_entered && !args.new {
        bail!(
            "already inside a Manifold session; use `mctl show` to present it or `mctl enter --new` to create a separate session"
        );
    }

    let target = resolve_control_target(args.dbus_name, args.dbus_address);
    let current_directory = current_directory()?;
    let existing_default = resolve_directory_surface_session(&target, &current_directory)?;
    let (session_id, set_default) = if args.new {
        let session_id = create_surface_session(&target, None)?;
        let status = status_snapshot(&target)?;
        eprintln!(
            "created Manifold session: {}",
            status.active_surface_session_name
        );
        (session_id, args.set_default || existing_default.is_none())
    } else {
        match existing_default {
            Some(session_id) => (session_id, false),
            None => (create_surface_session(&target, None)?, true),
        }
    };
    remember_directory_surface_session(&target, &current_directory, session_id, set_default)?;

    let status = status_snapshot(&target)?;

    let mut command = entered_command(args.command);
    command.env(
        MANIFOLD_SESSION_ENV,
        target.session_marker_with_session(Some(session_id)),
    );
    command.env(MANIFOLDD_DBUS_DESTINATION_ENV, &target.destination);
    command.env("MANIFOLD_SURFACE_SESSION_ID", session_id.to_string());
    if let Some(address) = target.address.as_deref() {
        command.env(MANIFOLDD_DBUS_ADDRESS_ENV, address);
    }

    if args.no_capture {
        if env::var_os("WAYLAND_DISPLAY").as_deref() == Some(status.wayland_display.as_ref()) {
            command.env_remove("WAYLAND_DISPLAY");
        }
    } else {
        command.env("WAYLAND_DISPLAY", &status.wayland_display);
    }

    let status = command.status().context("run entered command")?;
    match status.code() {
        Some(code) => std::process::exit(code),
        None => bail!("entered command terminated by signal"),
    }
}

pub fn show(args: ShowArgs) -> Result<()> {
    let target = resolve_control_target(args.dbus_name, args.dbus_address);
    let (session_id, current_directory, set_default) =
        resolve_cli_surface_session(&target, args.id, true, args.set_default)?;
    remember_directory_surface_session(&target, &current_directory, session_id, set_default)?;

    let status = status_snapshot(&target)?;
    switch_surface_session(&target, session_id)?;

    if status.presentation == "Detached" {
        let response = request_dbus_command(&target, &["attach".to_string()], false)?;
        ensure_ok_response(&response)?;
    }
    Ok(())
}

pub fn hide(args: HideArgs) -> Result<()> {
    let target = resolve_control_target(args.dbus_name, args.dbus_address);
    let (session_id, current_directory, set_default) =
        resolve_cli_surface_session(&target, args.id, false, false)?;
    remember_directory_surface_session(&target, &current_directory, session_id, set_default)?;

    let status = status_snapshot(&target)?;
    switch_surface_session(&target, session_id)?;

    if status.presentation == "Attached" {
        let response = request_dbus_command(&target, &["detach".to_string()], false)?;
        ensure_ok_response(&response)?;
    }
    Ok(())
}

pub fn list(args: ListArgs) -> Result<()> {
    let target = resolve_control_target(args.dbus_name, args.dbus_address);
    if args.global {
        let list = request_surface_session_list(&target)?;
        print_surface_sessions(&list.sessions, None);
        return Ok(());
    }

    let current_directory = current_directory()?;
    let list = request_directory_surface_session_list(&target, &current_directory)?;
    print_surface_sessions(&list.sessions, list.default);
    Ok(())
}

fn resolve_cli_surface_session(
    target: &ControlTarget,
    query: Option<String>,
    create_if_missing: bool,
    set_default_requested: bool,
) -> Result<(u64, PathBuf, bool)> {
    let current_directory = current_directory()?;
    let existing_default = resolve_directory_surface_session(target, &current_directory)?;
    let (session_id, set_default) = if let Some(query) = query {
        (
            resolve_surface_session(target, &query)?,
            set_default_requested,
        )
    } else if let Some(session_id) = existing_default {
        (session_id, false)
    } else if let Some(session_id) = entered_surface_session_id() {
        (session_id, true)
    } else if create_if_missing {
        (create_surface_session(target, None)?, true)
    } else {
        (status_snapshot(target)?.active_surface_session, false)
    };
    Ok((session_id, current_directory, set_default))
}

fn switch_surface_session(target: &ControlTarget, session_id: u64) -> Result<()> {
    let response = request_dbus_command(
        target,
        &[
            "surface-session".to_string(),
            "switch".to_string(),
            session_id.to_string(),
        ],
        false,
    )?;
    ensure_ok_response(&response)
}

fn current_directory() -> Result<PathBuf> {
    let directory = env::current_dir().context("determine current directory")?;
    Ok(directory.canonicalize().unwrap_or(directory))
}

fn create_surface_session(target: &ControlTarget, name: Option<String>) -> Result<u64> {
    let mut command = vec!["surface-session".to_string(), "new".to_string()];
    if let Some(name) = name {
        command.push(name);
    }
    let response = request_dbus_command(target, &command, false)?;
    ensure_ok_response(&response)?;

    let response = request_dbus_command(target, &[String::from("status-json")], false)?;
    let status: EnterStatusSnapshot =
        serde_json::from_str(response.trim()).context("decode Manifold status-json")?;
    Ok(status.active_surface_session)
}

fn status_snapshot(target: &ControlTarget) -> Result<EnterStatusSnapshot> {
    let response = request_dbus_command(target, &[String::from("status-json")], false)?;
    serde_json::from_str(response.trim()).context("decode Manifold status-json")
}

fn resolve_surface_session(target: &ControlTarget, query: &str) -> Result<u64> {
    let response = request_dbus_command(
        target,
        &[
            "surface-session".to_string(),
            "switch".to_string(),
            query.to_string(),
        ],
        false,
    )?;
    ensure_ok_response(&response)?;
    status_snapshot(target).map(|status| status.active_surface_session)
}

fn resolve_directory_surface_session(
    target: &ControlTarget,
    directory: &Path,
) -> Result<Option<u64>> {
    let response = request_dbus_command(
        target,
        &[
            "surface-session".to_string(),
            "dir-get".to_string(),
            directory.to_string_lossy().into_owned(),
        ],
        false,
    )?;
    parse_surface_session_lookup_response(&response)
}

fn remember_directory_surface_session(
    target: &ControlTarget,
    directory: &Path,
    session_id: u64,
    set_default: bool,
) -> Result<()> {
    let response = request_dbus_command(
        target,
        &[
            "surface-session".to_string(),
            "dir-set".to_string(),
            directory.to_string_lossy().into_owned(),
            session_id.to_string(),
            if set_default {
                "default"
            } else {
                "keep-default"
            }
            .to_string(),
        ],
        false,
    )?;
    ensure_ok_response(&response)
}

fn parse_surface_session_lookup_response(response: &str) -> Result<Option<u64>> {
    let response = response.trim();
    ensure!(
        response.starts_with("OK "),
        "control command failed: {response}"
    );
    let value = session_marker_value(response, "session")
        .context("surface-session dir-get response missing session marker")?;
    if value == "none" {
        return Ok(None);
    }
    let session_id = value
        .parse()
        .with_context(|| format!("parse surface session id {value:?}"))?;
    Ok(Some(session_id))
}

fn entered_session_marker() -> Option<String> {
    env::var(MANIFOLD_SESSION_ENV).ok().or_else(|| {
        env::var("MANIFOLD_SURFACE_SESSION_ID")
            .ok()
            .map(|session_id| format!("surface-session-id={session_id}"))
    })
}

fn entered_surface_session_id() -> Option<u64> {
    env::var("MANIFOLD_SURFACE_SESSION_ID")
        .ok()
        .and_then(|value| value.parse().ok())
        .or_else(|| {
            entered_session_marker().and_then(|value| {
                session_marker_value(&value, "surface-session-id")
                    .and_then(|value| value.parse().ok())
            })
        })
}

fn entered_command(argv: Vec<OsString>) -> ProcessCommand {
    let mut argv = argv.into_iter();
    let program = argv
        .next()
        .or_else(|| env::var_os("SHELL"))
        .unwrap_or_else(|| OsString::from("/bin/sh"));
    let mut command = ProcessCommand::new(&program);
    command.args(argv);
    command
}

pub fn switch(args: SwitchArgs) -> Result<()> {
    let target = resolve_control_target(args.dbus_name, args.dbus_address);
    let list = request_surface_session_list(&target)?;
    ensure!(!list.sessions.is_empty(), "no open sessions");
    let initial_query = joined_name(&args.query);

    let outcome = match args.session_id {
        Some(id) => {
            ensure!(
                list.sessions.iter().any(|session| session.id == id),
                "session {id} is not open"
            );
            SessionSwitchOutcome::Existing(id)
        }
        None => run_skim_session_picker(&args.skim, &list.sessions, initial_query.as_deref())?,
    };

    match outcome {
        SessionSwitchOutcome::Existing(selected) => {
            let response = request_dbus_command(
                &target,
                &[
                    "surface-session".to_string(),
                    "switch".to_string(),
                    selected.to_string(),
                ],
                false,
            )?;
            ensure_ok_response(&response)?;
        }
        SessionSwitchOutcome::Create(name) => {
            let response = request_dbus_command(
                &target,
                &["surface-session".to_string(), "new".to_string(), name],
                false,
            )?;
            ensure_ok_response(&response)?;
        }
    }

    if list.presentation == "Detached" {
        let response = request_dbus_command(&target, &["attach".to_string()], false)?;
        ensure_ok_response(&response)?;
    }

    Ok(())
}

pub(crate) fn resolve_control_target(
    destination: Option<String>,
    address: Option<String>,
) -> ControlTarget {
    let marker = env::var(MANIFOLD_SESSION_ENV).ok();
    let destination = destination
        .or_else(|| env::var(MANIFOLDD_DBUS_DESTINATION_ENV).ok())
        .or_else(|| {
            marker
                .as_deref()
                .and_then(|value| session_marker_value(value, "dbus-destination"))
        })
        .unwrap_or_else(|| DEFAULT_MANIFOLD_DBUS_NAME.to_string());
    let address = address
        .or_else(|| env::var(MANIFOLDD_DBUS_ADDRESS_ENV).ok())
        .or_else(|| {
            marker
                .as_deref()
                .and_then(|value| session_marker_value(value, "dbus-address"))
        });
    ControlTarget::new(destination, address)
}

fn session_marker_value(marker: &str, key: &str) -> Option<String> {
    marker.split_whitespace().find_map(|field| {
        field
            .strip_prefix(key)
            .and_then(|value| value.strip_prefix('='))
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

#[derive(Debug, serde::Deserialize)]
struct EnterStatusSnapshot {
    wayland_display: String,
    active_surface_session: u64,
    active_surface_session_name: String,
    presentation: String,
}

fn start_dbus_service(
    target: ControlTarget,
    sender: channel::Sender<DbusStateRequest>,
) -> Result<()> {
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::Builder::new()
        .name("manifold-dbus-control".to_string())
        .spawn(move || {
            let service_ready_tx = ready_tx.clone();
            let result = zbus::block_on(async move {
                let builder = match target.address.as_deref() {
                    Some(address) => zbus::connection::Builder::address(address)?,
                    None => zbus::connection::Builder::session()?,
                };
                let _connection = builder
                    .name(target.destination.as_str())?
                    .serve_at(
                        CONTROL_OBJECT_PATH,
                        ControlDbus {
                            sender: sender.clone(),
                        },
                    )?
                    .serve_at(
                        CONTROL_OBJECT_PATH,
                        SessionDbus {
                            sender: sender.clone(),
                        },
                    )?
                    .serve_at(
                        MANAGER_OBJECT_PATH,
                        ManagerDbus {
                            sender: sender.clone(),
                        },
                    )?
                    .serve_at(
                        EXTENSION_OBJECT_PATH,
                        ExtensionDbus {
                            sender: sender.clone(),
                        },
                    )?
                    .serve_at(COMPOSITOR_OBJECT_PATH, CompositorDbus { sender })?
                    .build()
                    .await?;
                service_ready_tx.send(Ok::<(), String>(())).ok();
                std::future::pending::<zbus::Result<()>>().await
            });
            let message = match result {
                Ok(()) => "DBus control service exited".to_string(),
                Err(err) => err.to_string(),
            };
            ready_tx.send(Err(message)).ok();
        })
        .context("spawn DBus control service thread")?;

    ready_rx
        .recv_timeout(CONTROL_REPLY_TIMEOUT)
        .context("DBus control service did not report readiness")?
        .map_err(anyhow::Error::msg)
}

pub fn request_dbus_command(
    target: &ControlTarget,
    command: &[String],
    json: bool,
) -> Result<String> {
    if json {
        let encoded = command.join(" ");
        let _: Vec<String> =
            serde_json::from_str(&encoded).context("validate JSON control argv")?;
    }

    let connection = match target.address.as_deref() {
        Some(address) => zbus::blocking::connection::Builder::address(address)
            .with_context(|| format!("parse DBus address {address}"))?
            .build()
            .with_context(|| format!("connect DBus address {address}"))?,
        None => zbus::blocking::Connection::session().context("connect session DBus")?,
    };
    let proxy = ManifoldControlProxyBlocking::builder(&connection)
        .destination(target.destination.as_str())?
        .path(CONTROL_OBJECT_PATH)?
        .interface(CONTROL_INTERFACE)?
        .build()
        .context("build Manifold DBus control proxy")?;
    proxy.command(command.to_vec(), json).with_context(|| {
        format!(
            "call Manifold DBus control command on {}",
            target.destination
        )
    })
}

fn handle_typed_ctl_command(target: &ControlTarget, command: &[String]) -> Result<bool> {
    let Some((command_name, args)) = command.split_first() else {
        return Ok(false);
    };
    if command_name != "extensions" {
        return Ok(false);
    }

    match args {
        [subcommand] if subcommand == "list" => {
            for (id, role, sender, metadata) in request_extension_list(target)? {
                println!(
                    "extension id={} role={} sender={} metadata={}",
                    id,
                    role,
                    sender,
                    format_metadata(&metadata)
                );
            }
        }
        [subcommand] if subcommand == "grants" => {
            for grant in request_grant_list(target, "")? {
                print_grant(&grant);
            }
        }
        [subcommand, extension_id] if subcommand == "grants" => {
            for grant in request_grant_list(target, extension_id)? {
                print_grant(&grant);
            }
        }
        _ => bail!("usage: extensions list | extensions grants [extension_id]"),
    }

    Ok(true)
}

fn typed_connection(target: &ControlTarget) -> Result<zbus::blocking::Connection> {
    match target.address.as_deref() {
        Some(address) => zbus::blocking::connection::Builder::address(address)
            .with_context(|| format!("parse DBus address {address}"))?
            .build()
            .with_context(|| format!("connect DBus address {address}")),
        None => zbus::blocking::Connection::session().context("connect session DBus"),
    }
}

fn request_extension_list(target: &ControlTarget) -> Result<Vec<DbusExtension>> {
    let connection = typed_connection(target)?;
    let proxy = ManifoldManagerProxyBlocking::builder(&connection)
        .destination(target.destination.as_str())?
        .path(MANAGER_OBJECT_PATH)?
        .interface(MANAGER_INTERFACE)?
        .build()
        .context("build Manifold Manager DBus proxy")?;
    proxy.list_extensions().with_context(|| {
        format!(
            "call Manifold Manager.ListExtensions on {}",
            target.destination
        )
    })
}

fn request_grant_list(target: &ControlTarget, extension_id: &str) -> Result<Vec<DbusGrant>> {
    let connection = typed_connection(target)?;
    let proxy = ManifoldExtensionProxyBlocking::builder(&connection)
        .destination(target.destination.as_str())?
        .path(EXTENSION_OBJECT_PATH)?
        .interface(EXTENSION_INTERFACE)?
        .build()
        .context("build Manifold Extension DBus proxy")?;
    proxy
        .list_grants(extension_id.to_string())
        .with_context(|| {
            format!(
                "call Manifold Extension.ListGrants on {}",
                target.destination
            )
        })
}

fn print_grant((id, extension_id, kind, sender, active, detail): &DbusGrant) {
    println!(
        "grant id={} extension_id={} kind={} sender={} active={} detail={}",
        id,
        extension_id,
        kind,
        sender,
        active,
        format_metadata(detail)
    );
}

fn format_metadata(metadata: &DbusMetadata) -> String {
    if metadata.is_empty() {
        return "{}".to_string();
    }
    let fields = metadata
        .iter()
        .map(|(key, value)| format!("{key}={}", sanitize_ctl_field(value)))
        .collect::<Vec<_>>();
    format!("{{{}}}", fields.join(","))
}

fn sanitize_ctl_field(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join("_")
}

pub fn execute_json_command(input: &str, state: &mut Manifold) -> Result<String> {
    let tokens = parse_command_tokens(input.trim())?;
    execute_tokens(&tokens, state)
}

fn execute_dbus_request(command: Vec<String>, json: bool, state: &mut Manifold) -> Result<String> {
    let tokens = if json {
        let encoded = command.join(" ");
        parse_command_tokens(encoded.trim())?
    } else {
        command
    };
    let response = execute_tokens(&tokens, state)?;

    if let Err(err) = state.display_handle.flush_clients() {
        warn!(?err, "flush clients after control command failed");
    }

    Ok(response)
}

fn execute_state_request(request: DbusStateRequest, state: &mut Manifold) {
    match request {
        DbusStateRequest::ControlCommand {
            command,
            json,
            response,
        } => send_response(response, execute_dbus_request(command, json, state)),
        DbusStateRequest::CurrentStatus { response } => {
            send_response(response, status_json_string(state))
        }
        DbusStateRequest::ListSurfaceSessions { response } => {
            state.refresh_window_state();
            let sessions = dbus_surface_sessions(state.surface_session_summaries());
            send_response(response, Ok(sessions));
        }
        DbusStateRequest::ActiveSession { response } => {
            send_response(response, Ok(state.active_surface_session().raw()));
        }
        DbusStateRequest::ResolveSession { query, response } => {
            let result = state
                .resolve_surface_session(&query)
                .map(SurfaceSessionId::raw)
                .with_context(|| format!("unknown session {query:?}"));
            send_response(response, result);
        }
        DbusStateRequest::ActivateSurfaceSession { id, response } => {
            let changed = state.switch_to_surface_session(SurfaceSessionId::from_raw(id));
            send_response(
                response,
                Ok(changed || state.active_surface_session().raw() == id),
            );
        }
        DbusStateRequest::CreateSurfaceSession { name, response } => {
            let id = if let Some(name) = normalize_session_name(&name) {
                state.create_and_attach_named_surface_session(Some(name)).raw()
            } else {
                WorkspaceCommand::Surface(SurfaceCommand::CreateAndAttachSession).apply(state);
                state.active_surface_session().raw()
            };
            send_response(response, Ok(id));
        }
        DbusStateRequest::RenameSurfaceSession { id, name, response } => {
            let changed = state.rename_surface_session(SurfaceSessionId::from_raw(id), name);
            send_response(response, Ok(changed));
        }
        DbusStateRequest::CloseSurfaceSession { id, response } => {
            let changed = state.close_surface_session(SurfaceSessionId::from_raw(id));
            send_response(response, Ok(changed));
        }
        DbusStateRequest::ListExtensions { response } => {
            let extensions = state
                .extension_summaries()
                .into_iter()
                .map(|extension| {
                    (
                        extension.id,
                        extension.role,
                        extension.sender,
                        extension.metadata,
                    )
                })
                .collect();
            send_response(response, Ok(extensions));
        }
        DbusStateRequest::RegisterExtension {
            role,
            metadata,
            sender_name,
            response,
        } => send_response(
            response,
            state.register_extension(role, metadata, sender_name),
        ),
        DbusStateRequest::UnregisterExtension {
            extension_id,
            response,
        } => send_response(response, Ok(state.unregister_extension(&extension_id))),
        DbusStateRequest::ListGrants {
            extension_id,
            response,
        } => {
            let grants = state
                .grant_summaries(&extension_id)
                .into_iter()
                .map(|grant| {
                    (
                        grant.id,
                        grant.extension_id,
                        grant.kind,
                        grant.sender,
                        grant.active,
                        grant.detail,
                    )
                })
                .collect();
            send_response(response, Ok(grants));
        }
        DbusStateRequest::ReleaseGrant { grant_id, response } => {
            send_response(response, Ok(state.release_grant(&grant_id)));
        }
        DbusStateRequest::RegisterWindow {
            extension_id,
            window_identity,
            role,
            sender_name,
            response,
        } => send_response(
            response,
            state.register_extension_window(extension_id, window_identity, role, sender_name),
        ),
        DbusStateRequest::RequestPlacement {
            window_id,
            placement,
            sender_name,
            response,
        } => send_response(
            response,
            state.request_window_placement(&window_id, placement, sender_name),
        ),
        DbusStateRequest::RequestPresentationMembership {
            window_id,
            membership,
            sender_name,
            response,
        } => send_response(
            response,
            state.request_presentation_membership(&window_id, membership, sender_name),
        ),
        DbusStateRequest::RequestShortcut {
            extension_id,
            shortcut,
            scope,
            label,
            sender_name,
            response,
        } => send_response(
            response,
            state.request_shortcut_grant(extension_id, shortcut, scope, label, sender_name),
        ),
        DbusStateRequest::RequestFocus {
            window_id,
            response,
        } => send_response(response, state.request_extension_window_focus(&window_id)),
        DbusStateRequest::Dismiss {
            window_id,
            response,
        } => send_response(response, state.dismiss_extension_window(&window_id)),
    }

    if let Err(err) = state.display_handle.flush_clients() {
        warn!(?err, "flush clients after DBus request failed");
    }
}

fn send_response<T>(response: mpsc::Sender<std::result::Result<T, String>>, result: Result<T>) {
    let response_result = result.map_err(|err| err.to_string());
    if let Err(err) = &response_result {
        warn!(error = %err, "DBus request failed");
    }
    let _ = response.send(response_result);
}

fn dbus_surface_sessions(sessions: Vec<SurfaceSessionSummary>) -> Vec<DbusSurfaceSession> {
    sessions
        .into_iter()
        .map(|session| {
            (
                session.id,
                session.name,
                session.active,
                u64::try_from(session.windows).unwrap_or(u64::MAX),
            )
        })
        .collect()
}

pub fn status_json_string(state: &mut Manifold) -> Result<String> {
    let snapshot = StatusSnapshot::from_state(state);
    serde_json::to_string(&snapshot.to_json()).context("serialize status snapshot")
}

fn execute_tokens(tokens: &[String], state: &mut Manifold) -> Result<String> {
    let Some((command, args)) = tokens.split_first() else {
        bail!("empty control command");
    };

    let response = match command.as_str() {
        "attach" if args.is_empty() => {
            state.request_attach();
            "OK attach requested\n".to_string()
        }
        "detach" if args.is_empty() => {
            state.request_detach();
            "OK detach requested\n".to_string()
        }
        "close-frontend" | "frontend-close" if args.is_empty() => {
            state.request_detach();
            "OK frontend close requested\n".to_string()
        }
        "status" if args.is_empty() => {
            let snapshot = StatusSnapshot::from_state(state);
            format!(
                "OK wayland_display={} wayland_client_insert_errors={} wayland_dispatch_errors={} xdg_configure_errors={} presentation={} presentation_target={} presentation_attach_failures={} presentation_runtime_failures={} presentation_last_error={} windows={} total_windows={} active_surface_session={} active_surface_session_name={} surface_sessions={} focused_windows={} focused_window_index={} focused_surface_id={} input_mode={} managed_fullscreen={} requested_fullscreen={} focused_fullscreen={} layout_regions={} layout_placeholders={} layout_active_kind={} layout_active_rect={} left_panel_visible={} left_panel_width={} browsers={} hosted_children={} hosted_child_exits={} hosted_child_failures={} hosted_child_last_status={} frames={} client_frame_callbacks={} commits={} pending_commits={} commit_frame_count={} last_commit_batch_commits={} max_commit_batch_commits={} last_commit_to_frame_us={} max_commit_to_frame_us={} last_commit_to_submit_us={} max_commit_to_submit_us={} client_frame_callback_commit_count={} last_client_frame_callback_batch_callbacks={} max_client_frame_callback_batch_callbacks={} last_client_frame_callback_to_commit_us={} max_client_frame_callback_to_commit_us={} client_frame_deferrals={} client_deferred_frames={} last_frame_us={} max_frame_us={} last_frame_interval_us={} max_frame_interval_us={} input_frame_deferrals={} input_deferred_frames={} input_events={} keyboard_events={} pointer_motion_events={} pointer_button_events={} pointer_axis_events={} pointer_target_hits={} pointer_target_misses={} focus_clears={} focus_repairs={} window_lifecycle_refreshes={} window_lifecycle_focus_clears={} window_lifecycle_focus_repairs={} pending_input_events={} input_frame_count={} last_input_batch_events={} max_input_batch_events={} last_input_to_frame_us={} max_input_to_frame_us={} last_input_to_submit_us={} max_input_to_submit_us={} input_commit_frame_count={} input_without_commit_frame_count={} last_input_to_commit_us={} max_input_to_commit_us={} last_input_commit_to_submit_us={} max_input_commit_to_submit_us={} attach_count={} detach_count={} last_attach_latency_us={} max_attach_latency_us={} last_attach_to_frame_us={} max_attach_to_frame_us={} last_detach_latency_us={} max_detach_latency_us={} shm_formats={} egl_wl_bind={} dmabuf={} dmabuf_formats={} dmabuf_disabled={} dmabuf_attempts={} dmabuf_accepted={} dmabuf_rejected={} dmabuf_last_import={} dmabuf_last_error={} dmabuf_explicit_sync_release_points={} dmabuf_explicit_sync_blockers={} dmabuf_implicit_blockers={} dmabuf_blocker_failures={} dmabuf_last_blocker_error={} render_node={} explicit_sync={} explicit_sync_disabled={}",
                snapshot.wayland_display,
                snapshot.wayland_client_insert_errors,
                snapshot.wayland_dispatch_errors,
                snapshot.xdg_configure_errors,
                snapshot.presentation,
                snapshot.presentation_target,
                snapshot.presentation_attach_failures,
                snapshot.presentation_runtime_failures,
                snapshot.presentation_last_error,
                snapshot.windows,
                snapshot.total_windows,
                snapshot.active_surface_session,
                snapshot.active_surface_session_name,
                snapshot.surface_sessions,
                snapshot.focused_windows,
                snapshot.focused_window_index_text,
                snapshot.focused_surface_id,
                snapshot.input_mode,
                snapshot.managed_fullscreen,
                snapshot.requested_fullscreen,
                snapshot.focused_fullscreen,
                snapshot.layout_regions,
                snapshot.layout_placeholders,
                snapshot.layout_active_kind,
                snapshot.layout_active_rect,
                snapshot.left_panel_visible,
                snapshot.left_panel_width,
                snapshot.browsers,
                snapshot.hosted_children,
                snapshot.hosted_child_exits,
                snapshot.hosted_child_failures,
                snapshot.hosted_child_last_status,
                snapshot.frames,
                snapshot.client_frame_callbacks,
                snapshot.commits,
                snapshot.pending_commits,
                snapshot.commit_frame_count,
                snapshot.last_commit_batch_commits,
                snapshot.max_commit_batch_commits,
                snapshot.last_commit_to_frame_us_text,
                snapshot.max_commit_to_frame_us,
                snapshot.last_commit_to_submit_us_text,
                snapshot.max_commit_to_submit_us,
                snapshot.client_frame_callback_commit_count,
                snapshot.last_client_frame_callback_batch_callbacks,
                snapshot.max_client_frame_callback_batch_callbacks,
                snapshot.last_client_frame_callback_to_commit_us_text,
                snapshot.max_client_frame_callback_to_commit_us,
                snapshot.client_frame_deferrals,
                snapshot.client_deferred_frames,
                snapshot.last_frame_us,
                snapshot.max_frame_us,
                snapshot.last_frame_interval_us,
                snapshot.max_frame_interval_us,
                snapshot.input_frame_deferrals,
                snapshot.input_deferred_frames,
                snapshot.input_events,
                snapshot.keyboard_events,
                snapshot.pointer_motion_events,
                snapshot.pointer_button_events,
                snapshot.pointer_axis_events,
                snapshot.pointer_target_hits,
                snapshot.pointer_target_misses,
                snapshot.focus_clears,
                snapshot.focus_repairs,
                snapshot.window_lifecycle_refreshes,
                snapshot.window_lifecycle_focus_clears,
                snapshot.window_lifecycle_focus_repairs,
                snapshot.pending_input_events,
                snapshot.input_frame_count,
                snapshot.last_input_batch_events,
                snapshot.max_input_batch_events,
                snapshot.last_input_to_frame_us_text,
                snapshot.max_input_to_frame_us,
                snapshot.last_input_to_submit_us_text,
                snapshot.max_input_to_submit_us,
                snapshot.input_commit_frame_count,
                snapshot.input_without_commit_frame_count,
                snapshot.last_input_to_commit_us_text,
                snapshot.max_input_to_commit_us,
                snapshot.last_input_commit_to_submit_us_text,
                snapshot.max_input_commit_to_submit_us,
                snapshot.attach_count,
                snapshot.detach_count,
                snapshot.last_attach_latency_us_text,
                snapshot.max_attach_latency_us,
                snapshot.last_attach_to_frame_us_text,
                snapshot.max_attach_to_frame_us,
                snapshot.last_detach_latency_us_text,
                snapshot.max_detach_latency_us,
                snapshot.shm_formats,
                snapshot.egl_wl_bind,
                snapshot.dmabuf,
                snapshot.dmabuf_formats,
                snapshot.dmabuf_disabled,
                snapshot.dmabuf_attempts,
                snapshot.dmabuf_accepted,
                snapshot.dmabuf_rejected,
                snapshot.dmabuf_last_import,
                snapshot.dmabuf_last_error,
                snapshot.dmabuf_explicit_sync_release_points,
                snapshot.dmabuf_explicit_sync_blockers,
                snapshot.dmabuf_implicit_blockers,
                snapshot.dmabuf_blocker_failures,
                snapshot.dmabuf_last_blocker_error,
                snapshot.render_node,
                snapshot.explicit_sync,
                snapshot.explicit_sync_disabled
            ) + "\n"
        }
        "status-json" if args.is_empty() => {
            format!("{}\n", status_json_string(state)?)
        }
        "quit" if args.is_empty() => {
            let closed_browsers = state.browsers.close_all().len();
            state.loop_signal.stop();
            format!("OK quit requested browsers_closed={closed_browsers}") + "\n"
        }
        "spawn" => {
            if args.is_empty() {
                bail!("spawn requires a command");
            }
            state.spawn_hosted_app_argv(args)?;
            format!("OK spawned {}\n", args.join(" "))
        }
        "focus" => {
            let direction = match args {
                [arg] if arg == "next" => FocusDirection::Next,
                [arg] if arg == "prev" || arg == "previous" => FocusDirection::Previous,
                _ => bail!("focus requires next or prev"),
            };
            let changed = state.cycle_focus(direction);
            format!("OK focus changed={changed}\n")
        }
        "layout-action" | "layout" => {
            let [action] = args else {
                bail!("layout-action requires exactly one action name");
            };
            let action = action
                .parse::<LayoutAction>()
                .with_context(|| format!("parse layout action {action:?}"))?;
            let changed = state.apply_layout_action(action);
            format!("OK layout-action action={action} changed={changed}\n")
        }
        "surface-session" => match args {
            [subcommand] if subcommand == "list-json" => {
                let response = SurfaceSessionList::from_state(state);
                format!("{}\n", serde_json::to_string(&response)?)
            }
            [subcommand, directory] if subcommand == "dir-list-json" => {
                let response = DirectorySurfaceSessionList::from_state(state, Path::new(directory));
                format!("{}\n", serde_json::to_string(&response)?)
            }
            [subcommand, directory] if subcommand == "dir-get" => {
                let session = state
                    .resolve_directory_surface_session(Path::new(directory))
                    .map(|session| session.raw());
                match session {
                    Some(session) => format!("OK surface-session session={session}\n"),
                    None => "OK surface-session session=none\n".to_string(),
                }
            }
            [subcommand, directory, id] if subcommand == "dir-set" => {
                let session = parse_u64(id, "session id")?;
                let changed = state.remember_directory_surface_session(
                    Path::new(directory).to_path_buf(),
                    SurfaceSessionId::from_raw(session),
                    true,
                );
                ensure!(changed, "unknown surface session {session}");
                format!("OK surface-session changed={changed} session={session}\n")
            }
            [subcommand, directory, id, mode] if subcommand == "dir-set" => {
                let session = parse_u64(id, "session id")?;
                let set_default = match mode.as_str() {
                    "default" | "set-default" => true,
                    "keep-default" | "no-default" => false,
                    _ => bail!("dir-set mode must be default or keep-default"),
                };
                let changed = state.remember_directory_surface_session(
                    Path::new(directory).to_path_buf(),
                    SurfaceSessionId::from_raw(session),
                    set_default,
                );
                ensure!(changed, "unknown surface session {session}");
                format!("OK surface-session changed={changed} session={session}\n")
            }
            [subcommand] if subcommand == "next" => {
                let changed =
                    WorkspaceCommand::Surface(SurfaceCommand::SwitchActiveSessionNext).apply(state);
                format!(
                    "OK surface-session active={} changed={changed}\n",
                    state.active_surface_session().raw()
                )
            }
            [subcommand, name @ ..] if subcommand == "new" => {
                let name = joined_name(name);
                if let Some(name) = name {
                    state.create_and_attach_named_surface_session(Some(name));
                } else {
                    WorkspaceCommand::Surface(SurfaceCommand::CreateAndAttachSession).apply(state);
                }
                format!(
                    "OK surface-session active={} created=true\n",
                    state.active_surface_session().raw()
                )
            }
            [subcommand, query] if subcommand == "switch" => {
                let session = state
                    .resolve_surface_session(query)
                    .with_context(|| format!("unknown surface session {query:?}"))?;
                let changed = state.switch_to_surface_session(session);
                ensure!(
                    changed || state.active_surface_session() == session,
                    "unknown surface session {query}"
                );
                format!(
                    "OK surface-session active={} changed={changed}\n",
                    state.active_surface_session().raw()
                )
            }
            _ => bail!(
                "usage: surface-session list-json|dir-list-json <dir>|dir-get <dir>|dir-set <dir> <id> [default|keep-default]|next|new|switch <query>"
            ),
        },
        "browser" => browser::handle_control(args, state)?,
        "harness" => match args {
            [subcommand, name] if subcommand == "signal" => {
                state.signal_test_harness(name.clone())?;
                format!("OK harness signal {}\n", name)
            }
            _ => bail!("usage: harness signal <name>"),
        },
        "nested-inner" => {
            let destination = state
                .nested_inner_dbus_destination
                .clone()
                .context("nested inner DBus destination not configured")?;
            if args.is_empty() {
                bail!("usage: nested-inner <command...>");
            }
            let target = ControlTarget::new(destination, state.nested_inner_dbus_address.clone());
            request_dbus_command(&target, args, false)?
        }
        "debug" => match args {
            [subcommand, dir] if subcommand == "dump" => {
                let manifest =
                    debug_dump::dump_bundle(state, Path::new(dir), DumpOptions::parse(None)?)?;
                format!("OK debug dump {}\n", manifest)
            }
            [subcommand, dir, selectors] if subcommand == "dump" => {
                let manifest = debug_dump::dump_bundle(
                    state,
                    Path::new(dir),
                    DumpOptions::parse(Some(selectors))?,
                )?;
                format!("OK debug dump {}\n", manifest)
            }
            _ => bail!("usage: debug dump <dir> [state,composed,surfaces]"),
        },
        "inject" => {
            handle_inject(args, state)?;
            format!("OK injected {}\n", args.join(" "))
        }
        _ => bail!("unknown command: {}", tokens.join(" ")),
    };

    Ok(response)
}

#[derive(serde::Serialize)]
struct StatusSnapshot {
    wayland_display: String,
    wayland_client_insert_errors: u64,
    wayland_dispatch_errors: u64,
    xdg_configure_errors: u64,
    presentation: &'static str,
    presentation_target: &'static str,
    presentation_attach_failures: u64,
    presentation_runtime_failures: u64,
    presentation_last_error: String,
    windows: usize,
    total_windows: usize,
    active_surface_session: u64,
    active_surface_session_name: String,
    surface_sessions: usize,
    focused_windows: usize,
    focused_window_index: Option<usize>,
    #[serde(skip)]
    focused_window_index_text: String,
    focused_surface_id: String,
    input_mode: &'static str,
    managed_fullscreen: bool,
    requested_fullscreen: usize,
    focused_fullscreen: bool,
    layout_regions: usize,
    layout_placeholders: usize,
    layout_active_kind: &'static str,
    layout_active_rect: String,
    left_panel_visible: bool,
    left_panel_width: i32,
    browsers: usize,
    hosted_children: usize,
    hosted_child_exits: u64,
    hosted_child_failures: u64,
    hosted_child_last_status: String,
    frames: u64,
    client_frame_callbacks: u64,
    commits: u64,
    pending_commits: u64,
    commit_frame_count: u64,
    last_commit_batch_commits: u64,
    max_commit_batch_commits: u64,
    last_commit_to_frame_us: Option<u128>,
    #[serde(skip)]
    last_commit_to_frame_us_text: String,
    max_commit_to_frame_us: u128,
    last_commit_to_submit_us: Option<u128>,
    #[serde(skip)]
    last_commit_to_submit_us_text: String,
    max_commit_to_submit_us: u128,
    client_frame_callback_commit_count: u64,
    last_client_frame_callback_batch_callbacks: u64,
    max_client_frame_callback_batch_callbacks: u64,
    last_client_frame_callback_to_commit_us: Option<u128>,
    #[serde(skip)]
    last_client_frame_callback_to_commit_us_text: String,
    max_client_frame_callback_to_commit_us: u128,
    client_frame_deferrals: u64,
    client_deferred_frames: u64,
    last_frame_us: u128,
    max_frame_us: u128,
    last_frame_interval_us: u128,
    max_frame_interval_us: u128,
    input_frame_deferrals: u64,
    input_deferred_frames: u64,
    input_events: u64,
    keyboard_events: u64,
    pointer_motion_events: u64,
    pointer_button_events: u64,
    pointer_axis_events: u64,
    pointer_target_hits: u64,
    pointer_target_misses: u64,
    focus_clears: u64,
    focus_repairs: u64,
    window_lifecycle_refreshes: u64,
    window_lifecycle_focus_clears: u64,
    window_lifecycle_focus_repairs: u64,
    pending_input_events: u64,
    input_frame_count: u64,
    last_input_batch_events: u64,
    max_input_batch_events: u64,
    last_input_to_frame_us: Option<u128>,
    #[serde(skip)]
    last_input_to_frame_us_text: String,
    max_input_to_frame_us: u128,
    last_input_to_submit_us: Option<u128>,
    #[serde(skip)]
    last_input_to_submit_us_text: String,
    max_input_to_submit_us: u128,
    input_commit_frame_count: u64,
    input_without_commit_frame_count: u64,
    last_input_to_commit_us: Option<u128>,
    #[serde(skip)]
    last_input_to_commit_us_text: String,
    max_input_to_commit_us: u128,
    last_input_commit_to_submit_us: Option<u128>,
    #[serde(skip)]
    last_input_commit_to_submit_us_text: String,
    max_input_commit_to_submit_us: u128,
    attach_count: u64,
    detach_count: u64,
    last_attach_latency_us: Option<u128>,
    #[serde(skip)]
    last_attach_latency_us_text: String,
    max_attach_latency_us: u128,
    last_attach_to_frame_us: Option<u128>,
    #[serde(skip)]
    last_attach_to_frame_us_text: String,
    max_attach_to_frame_us: u128,
    last_detach_latency_us: Option<u128>,
    #[serde(skip)]
    last_detach_latency_us_text: String,
    max_detach_latency_us: u128,
    shm_formats: usize,
    egl_wl_bind: String,
    dmabuf: String,
    dmabuf_formats: usize,
    dmabuf_disabled: &'static str,
    dmabuf_attempts: u64,
    dmabuf_accepted: u64,
    dmabuf_rejected: u64,
    dmabuf_last_import: String,
    dmabuf_last_error: String,
    dmabuf_explicit_sync_release_points: u64,
    dmabuf_explicit_sync_blockers: u64,
    dmabuf_implicit_blockers: u64,
    dmabuf_blocker_failures: u64,
    dmabuf_last_blocker_error: String,
    render_node: String,
    explicit_sync: String,
    explicit_sync_disabled: &'static str,
}

impl StatusSnapshot {
    fn from_state(state: &mut Manifold) -> Self {
        state.refresh_window_state();

        let last_input_to_frame_us = state.stats.last_input_to_frame_micros;
        let last_commit_to_frame_us = state.stats.last_commit_to_frame_micros;
        let last_input_to_submit_us = state.stats.last_input_to_submit_micros;
        let last_commit_to_submit_us = state.stats.last_commit_to_submit_micros;
        let last_input_to_commit_us = state.stats.last_input_to_commit_micros;
        let last_input_commit_to_submit_us = state.stats.last_input_commit_to_submit_micros;
        let last_client_frame_callback_to_commit_us =
            state.stats.last_client_frame_callback_to_commit_micros;
        let last_attach_latency_us = state.stats.last_attach_latency_micros;
        let last_attach_to_frame_us = state.stats.last_attach_to_frame_micros;
        let last_detach_latency_us = state.stats.last_detach_latency_micros;
        state.refresh_hosted_children();
        let presentation = match state.presentation {
            crate::state::PresentationStatus::Attached => "Attached",
            crate::state::PresentationStatus::Detached => "Detached",
        };
        let presentation_target = state
            .presenter
            .as_ref()
            .and_then(|presenter| {
                presenter
                    .runtime
                    .borrow()
                    .backend
                    .as_ref()
                    .map(|backend| backend.target_name())
            })
            .unwrap_or("none");
        let dmabuf = state
            .dmabuf
            .protocol_version
            .map_or_else(|| "none".to_string(), |version| format!("v{version}"));
        let explicit_sync = state
            .dmabuf
            .explicit_sync_protocol_version
            .map_or_else(|| "none".to_string(), |version| format!("v{version}"));
        let layout_active_rect = state.active_layout_rect().map_or_else(
            || "none".to_string(),
            |rect| format!("{},{},{},{}", rect.x, rect.y, rect.w, rect.h),
        );

        Self {
            wayland_display: state.socket_name.to_string_lossy().into_owned(),
            wayland_client_insert_errors: state.stats.wayland_client_insert_errors,
            wayland_dispatch_errors: state.stats.wayland_dispatch_errors,
            xdg_configure_errors: state.stats.xdg_configure_errors,
            presentation,
            presentation_target,
            presentation_attach_failures: state.presentation_attach_failures,
            presentation_runtime_failures: state.presentation_runtime_failures,
            presentation_last_error: state
                .presentation_last_error
                .as_deref()
                .unwrap_or("none")
                .to_string(),
            windows: state.active_surface_session_window_count(),
            total_windows: state.space.elements().count(),
            active_surface_session: state.active_surface_session().raw(),
            active_surface_session_name: sanitize_ctl_field(state.active_surface_session_name()),
            surface_sessions: state.surface_session_count(),
            focused_windows: state.focused_window_count(),
            focused_window_index: state.focused_window_index(),
            focused_window_index_text: state
                .focused_window_index()
                .map_or_else(|| "none".to_string(), |index| index.to_string()),
            focused_surface_id: state
                .focused_surface_id()
                .unwrap_or_else(|| "none".to_string()),
            input_mode: match state.input_mode {
                crate::state::InputMode::Passthrough => "passthrough",
                crate::state::InputMode::Normal => "normal",
            },
            managed_fullscreen: state.managed_fullscreen(),
            requested_fullscreen: state.requested_fullscreen_count(),
            focused_fullscreen: state.focused_window_fullscreen(),
            layout_regions: state.layout.region_count(),
            layout_placeholders: state.layout.placeholder_count(),
            layout_active_kind: match state.layout.active_kind() {
                Some(crate::layout::LayoutLeafKind::Client) => "client",
                Some(crate::layout::LayoutLeafKind::Placeholder) => "placeholder",
                None => "none",
            },
            layout_active_rect,
            left_panel_visible: state.left_panel_reserved_width() > 0,
            left_panel_width: state.left_panel_reserved_width(),
            browsers: state.browsers.len(),
            hosted_children: state.hosted_child_count(),
            hosted_child_exits: state.hosted_child_exits(),
            hosted_child_failures: state.hosted_child_failures(),
            hosted_child_last_status: sanitize_ctl_field(state.hosted_child_last_status()),
            frames: state.stats.frames_rendered,
            client_frame_callbacks: state.stats.client_frame_callbacks,
            commits: state.stats.commits,
            pending_commits: state.stats.pending_commits,
            commit_frame_count: state.stats.commit_frame_count,
            last_commit_batch_commits: state.stats.last_commit_batch_commits,
            max_commit_batch_commits: state.stats.max_commit_batch_commits,
            last_commit_to_frame_us,
            last_commit_to_frame_us_text: last_commit_to_frame_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_commit_to_frame_us: state.stats.max_commit_to_frame_micros,
            last_commit_to_submit_us,
            last_commit_to_submit_us_text: last_commit_to_submit_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_commit_to_submit_us: state.stats.max_commit_to_submit_micros,
            client_frame_callback_commit_count: state.stats.client_frame_callback_commit_count,
            last_client_frame_callback_batch_callbacks: state
                .stats
                .last_client_frame_callback_batch_callbacks,
            max_client_frame_callback_batch_callbacks: state
                .stats
                .max_client_frame_callback_batch_callbacks,
            last_client_frame_callback_to_commit_us,
            last_client_frame_callback_to_commit_us_text: last_client_frame_callback_to_commit_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_client_frame_callback_to_commit_us: state
                .stats
                .max_client_frame_callback_to_commit_micros,
            client_frame_deferrals: state.stats.client_frame_deferrals,
            client_deferred_frames: state.stats.client_deferred_frames,
            last_frame_us: state.stats.last_frame_micros,
            max_frame_us: state.stats.max_frame_micros,
            last_frame_interval_us: state.stats.last_frame_interval_micros,
            max_frame_interval_us: state.stats.max_frame_interval_micros,
            input_frame_deferrals: state.stats.input_frame_deferrals,
            input_deferred_frames: state.stats.input_deferred_frames,
            input_events: state.stats.input_events,
            keyboard_events: state.stats.keyboard_events,
            pointer_motion_events: state.stats.pointer_motion_events,
            pointer_button_events: state.stats.pointer_button_events,
            pointer_axis_events: state.stats.pointer_axis_events,
            pointer_target_hits: state.stats.pointer_target_hits,
            pointer_target_misses: state.stats.pointer_target_misses,
            focus_clears: state.stats.focus_clears,
            focus_repairs: state.stats.focus_repairs,
            window_lifecycle_refreshes: state.stats.window_lifecycle_refreshes,
            window_lifecycle_focus_clears: state.stats.window_lifecycle_focus_clears,
            window_lifecycle_focus_repairs: state.stats.window_lifecycle_focus_repairs,
            pending_input_events: state.stats.pending_input_events,
            input_frame_count: state.stats.input_frame_count,
            last_input_batch_events: state.stats.last_input_batch_events,
            max_input_batch_events: state.stats.max_input_batch_events,
            last_input_to_frame_us,
            last_input_to_frame_us_text: last_input_to_frame_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_input_to_frame_us: state.stats.max_input_to_frame_micros,
            last_input_to_submit_us,
            last_input_to_submit_us_text: last_input_to_submit_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_input_to_submit_us: state.stats.max_input_to_submit_micros,
            input_commit_frame_count: state.stats.input_commit_frame_count,
            input_without_commit_frame_count: state.stats.input_without_commit_frame_count,
            last_input_to_commit_us,
            last_input_to_commit_us_text: last_input_to_commit_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_input_to_commit_us: state.stats.max_input_to_commit_micros,
            last_input_commit_to_submit_us,
            last_input_commit_to_submit_us_text: last_input_commit_to_submit_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_input_commit_to_submit_us: state.stats.max_input_commit_to_submit_micros,
            attach_count: state.stats.attach_count,
            detach_count: state.stats.detach_count,
            last_attach_latency_us,
            last_attach_latency_us_text: last_attach_latency_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_attach_latency_us: state.stats.max_attach_latency_micros,
            last_attach_to_frame_us,
            last_attach_to_frame_us_text: last_attach_to_frame_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_attach_to_frame_us: state.stats.max_attach_to_frame_micros,
            last_detach_latency_us,
            last_detach_latency_us_text: last_detach_latency_us
                .map_or_else(|| "none".to_string(), |micros| micros.to_string()),
            max_detach_latency_us: state.stats.max_detach_latency_micros,
            shm_formats: state.dmabuf.shm_formats,
            egl_wl_bind: state.dmabuf.egl_wl_bind_status.clone(),
            dmabuf,
            dmabuf_formats: state.dmabuf.advertised_formats,
            dmabuf_disabled: state.dmabuf.disabled_reason.unwrap_or("none"),
            dmabuf_attempts: state.dmabuf.stats.attempts,
            dmabuf_accepted: state.dmabuf.stats.accepted,
            dmabuf_rejected: state.dmabuf.stats.rejected,
            dmabuf_last_import: state
                .dmabuf
                .stats
                .last_import
                .as_deref()
                .unwrap_or("none")
                .to_string(),
            dmabuf_last_error: state
                .dmabuf
                .stats
                .last_error
                .as_deref()
                .unwrap_or("none")
                .to_string(),
            dmabuf_explicit_sync_release_points: state.dmabuf.stats.explicit_sync_release_points,
            dmabuf_explicit_sync_blockers: state.dmabuf.stats.explicit_sync_blockers,
            dmabuf_implicit_blockers: state.dmabuf.stats.implicit_dmabuf_blockers,
            dmabuf_blocker_failures: state.dmabuf.stats.blocker_install_failures,
            dmabuf_last_blocker_error: state
                .dmabuf
                .stats
                .last_blocker_error
                .as_deref()
                .unwrap_or("none")
                .to_string(),
            render_node: state
                .dmabuf
                .render_node
                .as_deref()
                .map(sanitize_ctl_field)
                .unwrap_or_else(|| "none".to_string()),
            explicit_sync,
            explicit_sync_disabled: state.dmabuf.explicit_sync_disabled_reason,
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("serialize status snapshot")
    }
}

fn handle_inject(args: &[String], state: &mut Manifold) -> Result<()> {
    let Some((kind, rest)) = args.split_first() else {
        bail!("inject requires a kind: motion, button, key, or axis");
    };
    let time = monotonic_time_msec(state);

    match kind.as_str() {
        "motion" if rest.len() == 2 => {
            let x = parse_f64(&rest[0], "x")?;
            let y = parse_f64(&rest[1], "y")?;
            state.forward_pointer_motion(Point::from((x, y)), time);
        }
        "center-motion" if rest.is_empty() => {
            let Some(pos) = state.first_window_center() else {
                bail!("no mapped window to target");
            };
            state.forward_pointer_motion(pos, time);
        }
        "button" if rest.len() == 2 => {
            let button = parse_u32(&rest[0], "button")?;
            let button_state = parse_button_state(&rest[1])?;
            state.forward_pointer_button(button, button_state, time);
        }
        "key" if rest.len() == 2 => {
            let key = parse_u32(&rest[0], "key")?;
            let key_state = parse_key_state(&rest[1])?;
            state.forward_keyboard_key(key.into(), key_state, time);
        }
        "axis" if rest.len() == 2 => {
            let horizontal = parse_f64(&rest[0], "horizontal")?;
            let vertical = parse_f64(&rest[1], "vertical")?;
            let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
            if horizontal != 0.0 {
                frame = frame.value(Axis::Horizontal, horizontal);
            }
            if vertical != 0.0 {
                frame = frame.value(Axis::Vertical, vertical);
            }
            state.forward_pointer_axis(frame);
        }
        "wheel" if rest.len() == 2 => {
            let horizontal = parse_i32(&rest[0], "horizontal_v120")?;
            let vertical = parse_i32(&rest[1], "vertical_v120")?;
            let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
            if horizontal != 0 {
                frame = frame
                    .value(Axis::Horizontal, f64::from(horizontal) * 15.0 / 120.0)
                    .v120(Axis::Horizontal, horizontal);
            }
            if vertical != 0 {
                frame = frame
                    .value(Axis::Vertical, f64::from(vertical) * 15.0 / 120.0)
                    .v120(Axis::Vertical, vertical);
            }
            state.forward_pointer_axis(frame);
        }
        _ => bail!(
            "usage: inject motion <x> <y> | center-motion | button <code> <pressed|released> | key <code> <pressed|released> | axis <horizontal> <vertical> | wheel <horizontal_v120> <vertical_v120>"
        ),
    }
    if let Err(err) = state.display_handle.flush_clients() {
        tracing::warn!(?err, "flush clients after injected input failed");
    }

    Ok(())
}

fn monotonic_time_msec(state: &Manifold) -> u32 {
    let millis = state.start_time.elapsed().as_millis();
    u32::try_from(millis.min(u128::from(u32::MAX))).expect("millis clamped to u32 range")
}

fn parse_f64(value: &str, name: &str) -> Result<f64> {
    value
        .parse()
        .with_context(|| format!("{name} must be a number"))
}

fn parse_u32(value: &str, name: &str) -> Result<u32> {
    value
        .parse()
        .with_context(|| format!("{name} must be an unsigned integer"))
}

fn parse_u64(value: &str, name: &str) -> Result<u64> {
    value
        .parse()
        .with_context(|| format!("{name} must be an unsigned integer"))
}

fn parse_i32(value: &str, name: &str) -> Result<i32> {
    value
        .parse()
        .with_context(|| format!("{name} must be an integer"))
}

fn parse_button_state(value: &str) -> Result<ButtonState> {
    match value {
        "pressed" | "press" | "down" => Ok(ButtonState::Pressed),
        "released" | "release" | "up" => Ok(ButtonState::Released),
        _ => bail!("button state must be pressed or released"),
    }
}

fn parse_key_state(value: &str) -> Result<KeyState> {
    match value {
        "pressed" | "press" | "down" => Ok(KeyState::Pressed),
        "released" | "release" | "up" => Ok(KeyState::Released),
        _ => bail!("key state must be pressed or released"),
    }
}

fn parse_command_tokens(input: &str) -> Result<Vec<String>> {
    if input.starts_with('[') {
        let tokens: Vec<String> =
            serde_json::from_str(input).context("decode control command argv")?;
        if let [single] = tokens.as_slice() {
            return Ok(single.split_whitespace().map(ToString::to_string).collect());
        }
        return Ok(tokens);
    }

    Ok(input.split_whitespace().map(ToString::to_string).collect())
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct SurfaceSessionList {
    presentation: String,
    active: u64,
    sessions: Vec<SurfaceSessionSummary>,
}

impl SurfaceSessionList {
    fn from_state(state: &mut Manifold) -> Self {
        state.refresh_window_state();
        Self {
            presentation: match state.presentation {
                PresentationStatus::Attached => "Attached".to_string(),
                PresentationStatus::Detached => "Detached".to_string(),
            },
            active: state.active_surface_session().raw(),
            sessions: state.surface_session_summaries(),
        }
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct DirectorySurfaceSessionList {
    directory: String,
    default: Option<u64>,
    sessions: Vec<SurfaceSessionSummary>,
}

impl DirectorySurfaceSessionList {
    fn from_state(state: &mut Manifold, directory: &Path) -> Self {
        state.refresh_window_state();
        Self {
            directory: directory.to_string_lossy().into_owned(),
            default: state
                .resolve_directory_surface_session(directory)
                .map(SurfaceSessionId::raw),
            sessions: state.directory_surface_session_summaries(directory),
        }
    }
}

fn request_surface_session_list(target: &ControlTarget) -> Result<SurfaceSessionList> {
    let response = request_dbus_command(
        target,
        &["surface-session".to_string(), "list-json".to_string()],
        false,
    )?;
    serde_json::from_str(response.trim()).context("decode surface-session list")
}

fn request_directory_surface_session_list(
    target: &ControlTarget,
    directory: &Path,
) -> Result<DirectorySurfaceSessionList> {
    let response = request_dbus_command(
        target,
        &[
            "surface-session".to_string(),
            "dir-list-json".to_string(),
            directory.to_string_lossy().into_owned(),
        ],
        false,
    )?;
    serde_json::from_str(response.trim()).context("decode directory surface-session list")
}

fn print_surface_sessions(sessions: &[SurfaceSessionSummary], default: Option<u64>) {
    if sessions.is_empty() {
        println!("no sessions");
        return;
    }
    println!("ID\tNAME\tACTIVE\tWINDOWS\tDEFAULT");
    for session in sessions {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            session.id,
            session.name,
            if session.active { "yes" } else { "no" },
            session.windows,
            if Some(session.id) == default {
                "yes"
            } else {
                "no"
            },
        );
    }
}

enum SessionSwitchOutcome {
    Existing(u64),
    Create(String),
}

fn run_skim_session_picker(
    skim: &std::ffi::OsStr,
    sessions: &[SurfaceSessionSummary],
    initial_query: Option<&str>,
) -> Result<SessionSwitchOutcome> {
    if let Some(outcome) = resolve_initial_query(sessions, initial_query) {
        return Ok(outcome);
    }

    let mut child = ProcessCommand::new(skim)
        .arg("--prompt")
        .arg("manifold session> ")
        .arg("--print-query")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .args(initial_query_args(initial_query))
        .spawn()
        .with_context(|| format!("start skim command {}", skim.to_string_lossy()))?;

    {
        let stdin = child.stdin.as_mut().context("open skim stdin")?;
        for session in sessions {
            writeln!(stdin, "{}", format_session_picker_row(session))?;
        }
    }

    let output = child.wait_with_output().context("wait for skim")?;
    let selected = String::from_utf8(output.stdout).context("skim output must be utf-8")?;
    let outcome = parse_skim_session_output(selected.trim_end())?;
    ensure!(
        output.status.success() || matches!(outcome, SessionSwitchOutcome::Create(_)),
        "skim exited with status {}",
        output.status
    );
    Ok(outcome)
}

fn initial_query_args(initial_query: Option<&str>) -> Vec<String> {
    let Some(initial_query) = initial_query else {
        return Vec::new();
    };
    vec!["--query".to_string(), initial_query.to_string()]
}

fn resolve_initial_query(
    sessions: &[SurfaceSessionSummary],
    initial_query: Option<&str>,
) -> Option<SessionSwitchOutcome> {
    let query = normalize_session_name(initial_query?)?;
    let query_lower = query.to_lowercase();
    let matches = sessions
        .iter()
        .filter(|session| session.name.to_lowercase().contains(&query_lower))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Some(SessionSwitchOutcome::Create(query)),
        [session] => Some(SessionSwitchOutcome::Existing(session.id)),
        _ => None,
    }
}

fn format_session_picker_row(session: &SurfaceSessionSummary) -> String {
    let marker = if session.active { "*" } else { " " };
    let windows = match session.windows {
        1 => "1 window".to_string(),
        count => format!("{count} windows"),
    };
    format!(
        "{}\t{} {}  {}",
        session.id,
        marker,
        sanitize_session_picker_text(&session.name),
        windows
    )
}

fn parse_skim_session_output(output: &str) -> Result<SessionSwitchOutcome> {
    let mut lines = output.lines();
    let query = lines.next().unwrap_or_default().trim();
    let selected = lines.next().unwrap_or_default().trim();
    if selected.is_empty() {
        let name = normalize_session_name(query).context("no session selected")?;
        return Ok(SessionSwitchOutcome::Create(name));
    }
    parse_selected_session_id(selected).map(SessionSwitchOutcome::Existing)
}

fn parse_selected_session_id(selected: &str) -> Result<u64> {
    let Some(id) = selected.split('\t').next().filter(|id| !id.is_empty()) else {
        bail!("no session selected");
    };
    parse_u64(id, "selected session id")
}

fn joined_name(parts: &[String]) -> Option<String> {
    normalize_session_name(&parts.join(" "))
}

fn normalize_session_name(name: &str) -> Option<String> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty()).then_some(name)
}

fn sanitize_session_picker_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn ensure_ok_response(response: &str) -> Result<()> {
    ensure!(
        response.starts_with("OK "),
        "control command failed: {}",
        response.trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ControlTarget, SessionSwitchOutcome, format_session_picker_row, parse_skim_session_output,
        resolve_initial_query, session_marker_value,
    };
    use crate::state::SurfaceSessionSummary;

    fn session(id: u64, name: &str) -> SurfaceSessionSummary {
        SurfaceSessionSummary {
            id,
            name: name.to_string(),
            active: false,
            windows: 0,
        }
    }

    #[test]
    fn picker_row_starts_with_machine_readable_session_id() {
        let row = format_session_picker_row(&SurfaceSessionSummary {
            id: 7,
            name: "notes".to_string(),
            active: true,
            windows: 2,
        });

        let output = format!("ignored query\n{row}");
        let outcome = parse_skim_session_output(&output).unwrap();
        assert!(matches!(outcome, SessionSwitchOutcome::Existing(7)));
        assert!(row.contains("* notes"));
        assert!(row.contains("2 windows"));
    }

    #[test]
    fn query_without_selection_creates_named_session() {
        let outcome = parse_skim_session_output("research notes").unwrap();

        assert!(matches!(
            outcome,
            SessionSwitchOutcome::Create(name) if name == "research notes"
        ));
    }

    #[test]
    fn initial_query_selects_single_name_match() {
        let sessions = vec![session(1, "session 1"), session(2, "research notes")];
        let outcome = resolve_initial_query(&sessions, Some("research")).unwrap();

        assert!(matches!(outcome, SessionSwitchOutcome::Existing(2)));
    }

    #[test]
    fn initial_query_creates_when_name_matches_nothing() {
        let sessions = vec![session(1, "session 1")];
        let outcome = resolve_initial_query(&sessions, Some("research notes")).unwrap();

        assert!(matches!(
            outcome,
            SessionSwitchOutcome::Create(name) if name == "research notes"
        ));
    }

    #[test]
    fn empty_query_without_selection_is_cancelled_selection() {
        assert!(parse_skim_session_output("").is_err());
    }

    #[test]
    fn session_marker_includes_surface_session_id() {
        let marker = ControlTarget::new("org.manifold.Session".to_string(), None)
            .session_marker_with_session(Some(42));

        assert_eq!(
            session_marker_value(&marker, "surface-session-id"),
            Some("42".to_string())
        );
    }
}
