#![allow(
    clippy::exit,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::let_underscore_must_use,
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::option_if_let_else,
    clippy::too_many_arguments,
    clippy::unnecessary_wraps,
    reason = "tracked in issues.md: control.rs preserves token-oriented command plumbing while DBus waits cross the compositor actor boundary"
)]
// TODO(lint-structure): separate DBus proxy ABI, CLI parsing, and process entrypoint policy.

use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

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
use zbus::{fdo, message::Header, object_server::SignalEmitter, proxy};

use crate::{
    CtlArgs, DEFAULT_MANIFOLD_DBUS_NAME, EnterArgs, HideArgs, ListArgs, MANIFOLD_SESSION_ENV,
    MANIFOLDD_DBUS_ADDRESS_ENV, MANIFOLDD_DBUS_DESTINATION_ENV, PlugArgs, ShowArgs, SwitchArgs,
    browser,
    command::{SurfaceCommand, WorkspaceCommand},
    config,
    debug_dump::{self, DumpOptions},
    layout::LayoutAction,
    session_runner,
    state::{
        FocusDirection, MANIFOLD_STATUS_PUBLISHER, Manifold, PASSTHROUGH_STATUS_CHANNEL,
        PresentationStatus, SurfaceSessionId, SurfaceSessionSummary,
    },
};

const CONTROL_OBJECT_PATH: &str = "/org/manifold/Session";
const MANAGER_OBJECT_PATH: &str = "/org/manifold/Manager";
const EXTENSION_OBJECT_PATH: &str = "/org/manifold/Extension";
const COMPOSITOR_OBJECT_PATH: &str = "/org/manifold/Compositor";
const AGENT_OBJECT_PATH: &str = "/org/manifold/Agent";
const STATUS_OBJECT_PATH: &str = "/org/manifold/Status";
const CONTROL_INTERFACE: &str = "org.manifold.Session.Control";
const MANAGER_INTERFACE: &str = "org.manifold.Manager";
const EXTENSION_INTERFACE: &str = "org.manifold.Extension";
const AGENT_INTERFACE: &str = "org.manifold.Agent";
const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(15);

type DbusMetadata = BTreeMap<String, String>;
type DbusSurfaceSession = (u64, String, bool, u64);
type DbusExtension = (String, String, String, DbusMetadata);
type DbusGrant = (String, String, String, String, bool, DbusMetadata);
type DbusStatusItem = (String, String, String, String, DbusMetadata);
type DbusAgent = (
    String,
    String,
    String,
    String,
    u64,
    bool,
    u64,
    u64,
    String,
    String,
    DbusMetadata,
);

#[derive(Clone, Debug)]
pub struct ControlTarget {
    pub(crate) destination: String,
    pub(crate) address: Option<String>,
}

impl ControlTarget {
    pub(crate) const fn new(destination: String, address: Option<String>) -> Self {
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

type DbusResponse<T> = tokio::sync::oneshot::Sender<std::result::Result<T, String>>;
type ControlResponse = DbusResponse<String>;
type BoolResponse = DbusResponse<bool>;
type U64Response = DbusResponse<u64>;
type SessionListResponse = DbusResponse<Vec<DbusSurfaceSession>>;
type ExtensionListResponse = DbusResponse<Vec<DbusExtension>>;
type GrantListResponse = DbusResponse<Vec<DbusGrant>>;
type StatusListResponse = DbusResponse<Vec<DbusStatusItem>>;
type AgentListResponse = DbusResponse<Vec<DbusAgent>>;
type OptionalControlResponse = DbusResponse<Option<String>>;

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
    ListStatus {
        response: StatusListResponse,
    },
    RegisterStatusPublisher {
        role: String,
        metadata: DbusMetadata,
        sender_name: String,
        response: ControlResponse,
    },
    PublishStatus {
        publisher: String,
        channel: String,
        state: String,
        label: String,
        metadata: DbusMetadata,
        response: BoolResponse,
    },
    ClearStatus {
        publisher: String,
        channel: String,
        response: BoolResponse,
    },
    SetPassthroughMode {
        enabled: bool,
        response: BoolResponse,
    },
    ListAgents {
        response: AgentListResponse,
    },
    RegisterAgent {
        kind: String,
        metadata: DbusMetadata,
        sender_name: String,
        response: ControlResponse,
    },
    UpdateAgent {
        agent_id: String,
        status: String,
        metadata: DbusMetadata,
        sender_name: String,
        response: BoolResponse,
    },
    NotifyAgent {
        agent_id: String,
        title: String,
        subtitle: String,
        body: String,
        level: String,
        metadata: DbusMetadata,
        sender_name: String,
        response: ControlResponse,
    },
    ClearAgentAttention {
        agent_id: String,
        response: BoolResponse,
    },
    ShouldNotifyAgentDesktop {
        agent_id: String,
        response: BoolResponse,
    },
    UnregisterAgent {
        agent_id: String,
        response: BoolResponse,
    },
    UnregisterAgentByIdentity {
        kind: String,
        metadata: DbusMetadata,
        response: OptionalControlResponse,
    },
    RegisterWindow {
        extension_id: String,
        window_identity: DbusMetadata,
        role: String,
        sender_name: String,
        response: ControlResponse,
    },
    ReplaceRole {
        window_id: String,
        role: String,
        options: DbusMetadata,
        sender_name: String,
        response: BoolResponse,
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
    FocusPresentationWindow {
        response: BoolResponse,
    },
    FocusPresentationWindowWithActivationToken {
        activation_token: String,
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
struct AgentDbus {
    sender: channel::Sender<DbusStateRequest>,
}

#[derive(Clone)]
struct StatusDbus {
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
        .await
    }
}

#[zbus::interface(name = "org.manifold.Manager")]
impl ManagerDbus {
    async fn list_sessions(&self) -> fdo::Result<Vec<DbusSurfaceSession>> {
        request_state(&self.sender, |response| {
            DbusStateRequest::ListSurfaceSessions { response }
        })
        .await
    }

    async fn active_session(&self) -> fdo::Result<u64> {
        request_state(&self.sender, |response| DbusStateRequest::ActiveSession {
            response,
        })
        .await
    }

    async fn resolve_session(&self, session: String) -> fdo::Result<u64> {
        request_state(&self.sender, |response| DbusStateRequest::ResolveSession {
            query: session,
            response,
        })
        .await
    }

    async fn list_extensions(&self) -> fdo::Result<Vec<DbusExtension>> {
        request_state(&self.sender, |response| DbusStateRequest::ListExtensions {
            response,
        })
        .await
    }
}

#[zbus::interface(name = "org.manifold.Session")]
impl SessionDbus {
    async fn current_status(&self) -> fdo::Result<String> {
        request_state(&self.sender, |response| DbusStateRequest::CurrentStatus {
            response,
        })
        .await
    }

    async fn list_surface_sessions(&self) -> fdo::Result<Vec<DbusSurfaceSession>> {
        request_state(&self.sender, |response| {
            DbusStateRequest::ListSurfaceSessions { response }
        })
        .await
    }

    async fn activate_surface_session(&self, id: u64) -> fdo::Result<bool> {
        request_state(&self.sender, |response| {
            DbusStateRequest::ActivateSurfaceSession { id, response }
        })
        .await
    }

    async fn create_surface_session(&self, name: String) -> fdo::Result<u64> {
        request_state(&self.sender, |response| {
            DbusStateRequest::CreateSurfaceSession { name, response }
        })
        .await
    }

    async fn rename_surface_session(&self, id: u64, name: String) -> fdo::Result<bool> {
        request_state(&self.sender, |response| {
            DbusStateRequest::RenameSurfaceSession { id, name, response }
        })
        .await
    }

    async fn close_surface_session(&self, id: u64) -> fdo::Result<bool> {
        request_state(&self.sender, |response| {
            DbusStateRequest::CloseSurfaceSession { id, response }
        })
        .await
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
        .await
    }

    async fn unregister_extension(&self, extension_id: String) -> fdo::Result<bool> {
        request_state(&self.sender, |response| {
            DbusStateRequest::UnregisterExtension {
                extension_id,
                response,
            }
        })
        .await
    }

    async fn list_grants(&self, extension_id: String) -> fdo::Result<Vec<DbusGrant>> {
        request_state(&self.sender, |response| DbusStateRequest::ListGrants {
            extension_id,
            response,
        })
        .await
    }

    async fn release_grant(&self, grant_id: String) -> fdo::Result<bool> {
        request_state(&self.sender, |response| DbusStateRequest::ReleaseGrant {
            grant_id,
            response,
        })
        .await
    }
}

#[zbus::interface(name = "org.manifold.Agent")]
impl AgentDbus {
    async fn list_agents(&self) -> fdo::Result<Vec<DbusAgent>> {
        request_state(&self.sender, |response| DbusStateRequest::ListAgents {
            response,
        })
        .await
    }

    async fn register_agent(
        &self,
        kind: String,
        metadata: DbusMetadata,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        let sender_name = header_sender(&header);
        let agent_id = request_state(&self.sender, |response| DbusStateRequest::RegisterAgent {
            kind,
            metadata,
            sender_name: sender_name.clone(),
            response,
        })
        .await?;
        Self::agent_registered(&emitter, &agent_id, &sender_name)
            .await
            .map_err(|err| fdo::Error::Failed(err.to_string()))?;
        Ok(agent_id)
    }

    async fn update_agent(
        &self,
        agent_id: String,
        status: String,
        metadata: DbusMetadata,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<bool> {
        let sender_name = header_sender(&header);
        let changed = request_state(&self.sender, |response| DbusStateRequest::UpdateAgent {
            agent_id: agent_id.clone(),
            status,
            metadata,
            sender_name: sender_name.clone(),
            response,
        })
        .await?;
        if changed {
            Self::agent_updated(&emitter, &agent_id, &sender_name)
                .await
                .map_err(|err| fdo::Error::Failed(err.to_string()))?;
        }
        Ok(changed)
    }

    async fn notify_agent(
        &self,
        agent_id: String,
        title: String,
        subtitle: String,
        body: String,
        level: String,
        metadata: DbusMetadata,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        let sender_name = header_sender(&header);
        let notification_id =
            request_state(&self.sender, |response| DbusStateRequest::NotifyAgent {
                agent_id: agent_id.clone(),
                title: title.clone(),
                subtitle,
                body: body.clone(),
                level: level.clone(),
                metadata,
                sender_name: sender_name.clone(),
                response,
            })
            .await?;
        Self::agent_notification(
            &emitter,
            &agent_id,
            &notification_id,
            &title,
            &body,
            &level,
            &sender_name,
        )
        .await
        .map_err(|err| fdo::Error::Failed(err.to_string()))?;
        Ok(notification_id)
    }

    async fn clear_agent_attention(
        &self,
        agent_id: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<bool> {
        let changed = request_state(&self.sender, |response| {
            DbusStateRequest::ClearAgentAttention {
                agent_id: agent_id.clone(),
                response,
            }
        })
        .await?;
        if changed {
            Self::agent_attention_cleared(&emitter, &agent_id)
                .await
                .map_err(|err| fdo::Error::Failed(err.to_string()))?;
        }
        Ok(changed)
    }

    async fn should_notify_agent_desktop(&self, agent_id: String) -> fdo::Result<bool> {
        request_state(&self.sender, |response| {
            DbusStateRequest::ShouldNotifyAgentDesktop { agent_id, response }
        })
        .await
    }

    async fn unregister_agent(
        &self,
        agent_id: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<bool> {
        let changed = request_state(&self.sender, |response| DbusStateRequest::UnregisterAgent {
            agent_id: agent_id.clone(),
            response,
        })
        .await?;
        if changed {
            Self::agent_unregistered(&emitter, &agent_id)
                .await
                .map_err(|err| fdo::Error::Failed(err.to_string()))?;
        }
        Ok(changed)
    }

    async fn unregister_agent_by_identity(
        &self,
        kind: String,
        metadata: DbusMetadata,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<bool> {
        let removed = request_state(&self.sender, |response| {
            DbusStateRequest::UnregisterAgentByIdentity {
                kind,
                metadata,
                response,
            }
        })
        .await?;
        if let Some(agent_id) = removed {
            Self::agent_unregistered(&emitter, &agent_id)
                .await
                .map_err(|err| fdo::Error::Failed(err.to_string()))?;
            return Ok(true);
        }
        Ok(false)
    }

    #[zbus(signal)]
    async fn agent_registered(
        emitter: &SignalEmitter<'_>,
        agent_id: &str,
        sender: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn agent_updated(
        emitter: &SignalEmitter<'_>,
        agent_id: &str,
        sender: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn agent_notification(
        emitter: &SignalEmitter<'_>,
        agent_id: &str,
        notification_id: &str,
        title: &str,
        body: &str,
        level: &str,
        sender: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn agent_attention_cleared(
        emitter: &SignalEmitter<'_>,
        agent_id: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn agent_unregistered(emitter: &SignalEmitter<'_>, agent_id: &str) -> zbus::Result<()>;
}

#[zbus::interface(name = "org.manifold.Status")]
impl StatusDbus {
    async fn list_status(&self) -> fdo::Result<Vec<DbusStatusItem>> {
        request_state(&self.sender, |response| DbusStateRequest::ListStatus {
            response,
        })
        .await
    }

    async fn register_publisher(
        &self,
        role: String,
        metadata: DbusMetadata,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<String> {
        let sender_name = header_sender(&header);
        request_state(&self.sender, |response| {
            DbusStateRequest::RegisterStatusPublisher {
                role,
                metadata,
                sender_name,
                response,
            }
        })
        .await
    }

    async fn publish(
        &self,
        publisher: String,
        channel: String,
        state: String,
        label: String,
        metadata: DbusMetadata,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<bool> {
        let changed = request_state(&self.sender, |response| DbusStateRequest::PublishStatus {
            publisher: publisher.clone(),
            channel: channel.clone(),
            state: state.clone(),
            label: label.clone(),
            metadata: metadata.clone(),
            response,
        })
        .await?;
        if changed {
            Self::status_changed(&emitter, &channel, &state, &label, &publisher, &metadata)
                .await
                .map_err(|err| fdo::Error::Failed(err.to_string()))?;
        }
        Ok(changed)
    }

    async fn clear(
        &self,
        publisher: String,
        channel: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<bool> {
        let changed = request_state(&self.sender, |response| DbusStateRequest::ClearStatus {
            publisher: publisher.clone(),
            channel: channel.clone(),
            response,
        })
        .await?;
        if changed {
            Self::status_cleared(&emitter, &channel, &publisher)
                .await
                .map_err(|err| fdo::Error::Failed(err.to_string()))?;
        }
        Ok(changed)
    }

    async fn set_passthrough_mode(
        &self,
        enabled: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<bool> {
        let changed = request_state(&self.sender, |response| {
            DbusStateRequest::SetPassthroughMode { enabled, response }
        })
        .await?;
        if changed && enabled {
            let mut metadata = DbusMetadata::new();
            metadata.insert("scope".to_string(), "workspace".to_string());
            metadata.insert("click_action".to_string(), "disable".to_string());
            Self::status_changed(
                &emitter,
                PASSTHROUGH_STATUS_CHANNEL,
                "active",
                "Passthrough",
                MANIFOLD_STATUS_PUBLISHER,
                &metadata,
            )
            .await
            .map_err(|err| fdo::Error::Failed(err.to_string()))?;
        } else if changed {
            Self::status_cleared(
                &emitter,
                PASSTHROUGH_STATUS_CHANNEL,
                MANIFOLD_STATUS_PUBLISHER,
            )
            .await
            .map_err(|err| fdo::Error::Failed(err.to_string()))?;
        }
        Ok(changed)
    }

    #[zbus(signal)]
    async fn status_changed(
        emitter: &SignalEmitter<'_>,
        channel: &str,
        state: &str,
        label: &str,
        publisher: &str,
        metadata: &DbusMetadata,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_cleared(
        emitter: &SignalEmitter<'_>,
        channel: &str,
        publisher: &str,
    ) -> zbus::Result<()>;
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
        .await
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
        .await
    }

    async fn replace_role(
        &self,
        window_id: String,
        role: String,
        options: DbusMetadata,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<bool> {
        let sender_name = header_sender(&header);
        request_state(&self.sender, |response| DbusStateRequest::ReplaceRole {
            window_id,
            role,
            options,
            sender_name,
            response,
        })
        .await
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
        .await
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
        .await
    }

    async fn request_focus(&self, window_id: String) -> fdo::Result<bool> {
        request_state(&self.sender, |response| DbusStateRequest::RequestFocus {
            window_id,
            response,
        })
        .await
    }

    async fn focus_presentation_window(&self) -> fdo::Result<bool> {
        request_state(&self.sender, |response| {
            DbusStateRequest::FocusPresentationWindow { response }
        })
        .await
    }

    async fn focus_presentation_window_with_activation_token(
        &self,
        activation_token: String,
    ) -> fdo::Result<bool> {
        request_state(&self.sender, |response| {
            DbusStateRequest::FocusPresentationWindowWithActivationToken {
                activation_token,
                response,
            }
        })
        .await
    }

    async fn dismiss(&self, window_id: String) -> fdo::Result<bool> {
        request_state(&self.sender, |response| DbusStateRequest::Dismiss {
            window_id,
            response,
        })
        .await
    }
}

fn request_state<T>(
    sender: &channel::Sender<DbusStateRequest>,
    build: impl FnOnce(DbusResponse<T>) -> DbusStateRequest,
) -> impl Future<Output = fdo::Result<T>> {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    let send_result = sender.send(build(response_tx));
    async move {
        send_result.map_err(|err| {
            fdo::Error::Failed(format!("Manifold session loop is not running: {err}"))
        })?;
        tokio::time::timeout(CONTROL_REPLY_TIMEOUT, response_rx)
            .await
            .map_err(|err| {
                fdo::Error::Failed(format!(
                    "timed out waiting for Manifold control response: {err}"
                ))
            })?
            .map_err(|err| {
                fdo::Error::Failed(format!("Manifold control response was dropped: {err}"))
            })?
            .map_err(fdo::Error::Failed)
    }
}

fn header_sender(header: &Header<'_>) -> String {
    header
        .sender()
        .map_or_else(|| "unknown".to_string(), ToString::to_string)
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

#[proxy(interface = "org.manifold.Agent", default_path = "/org/manifold/Agent")]
trait ManifoldAgent {
    fn list_agents(&self) -> zbus::Result<Vec<DbusAgent>>;
    fn register_agent(&self, kind: String, metadata: DbusMetadata) -> zbus::Result<String>;
    fn update_agent(
        &self,
        agent_id: String,
        status: String,
        metadata: DbusMetadata,
    ) -> zbus::Result<bool>;
    fn notify_agent(
        &self,
        agent_id: String,
        title: String,
        subtitle: String,
        body: String,
        level: String,
        metadata: DbusMetadata,
    ) -> zbus::Result<String>;
    fn clear_agent_attention(&self, agent_id: String) -> zbus::Result<bool>;
    fn should_notify_agent_desktop(&self, agent_id: String) -> zbus::Result<bool>;
    fn unregister_agent(&self, agent_id: String) -> zbus::Result<bool>;
}

#[proxy(
    interface = "org.manifold.Status",
    default_path = "/org/manifold/Status"
)]
trait ManifoldStatus {
    fn list_status(&self) -> zbus::Result<Vec<DbusStatusItem>>;
    fn register_publisher(&self, role: String, metadata: DbusMetadata) -> zbus::Result<String>;
    fn publish(
        &self,
        publisher: String,
        channel: String,
        state: String,
        label: String,
        metadata: DbusMetadata,
    ) -> zbus::Result<bool>;
    fn clear(&self, publisher: String, channel: String) -> zbus::Result<bool>;
    fn set_passthrough_mode(&self, enabled: bool) -> zbus::Result<bool>;
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
    fn replace_role(
        &self,
        window_id: String,
        role: String,
        options: DbusMetadata,
    ) -> zbus::Result<bool>;
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
    fn focus_presentation_window(&self) -> zbus::Result<bool>;
    fn focus_presentation_window_with_activation_token(
        &self,
        activation_token: String,
    ) -> zbus::Result<bool>;
    fn dismiss(&self, window_id: String) -> zbus::Result<bool>;
}

pub fn listen(
    event_loop: &mut EventLoop<'static, Manifold>,
    target: ControlTarget,
    runtime: tokio::runtime::Handle,
) -> Result<()> {
    let (sender, receiver) = channel::channel::<DbusStateRequest>();
    event_loop
        .handle()
        .insert_source(receiver, |event, (), state| {
            let Event::Msg(request) = event else {
                return;
            };
            execute_state_request(request, state);
        })
        .map_err(|err| anyhow::anyhow!("insert DBus control request channel: {err:?}"))?;

    start_dbus_service(target.clone(), sender, runtime)?;

    info!(
        dbus_destination = %target.destination,
        dbus_address = ?target.address,
        "DBus control service listening"
    );
    Ok(())
}

pub fn send(args: CtlArgs) -> Result<()> {
    let context = open_cli_control_context(args.dbus_name, args.dbus_address)?;
    let target = &context.target;
    if !args.json && handle_typed_ctl_command(target, &args.command)? {
        return Ok(());
    }
    let command = control_command_with_target_session(
        target,
        &context.current_directory,
        args.command,
        args.json,
    )?;
    let response = request_dbus_command(target, &command, args.json)?;
    print!("{response}");
    Ok(())
}

pub fn plug(args: PlugArgs) -> Result<()> {
    let context = open_cli_control_context(args.dbus_name, args.dbus_address)?;
    let path = args
        .path
        .canonicalize()
        .with_context(|| format!("resolve WASM module {}", args.path.display()))?;
    let response = request_dbus_command(
        &context.target,
        &[
            "plug".to_string(),
            "--kind".to_string(),
            args.kind,
            path.display().to_string(),
        ],
        false,
    )?;
    ensure_ok_response(&response)?;
    print!("{response}");
    Ok(())
}

pub fn enter(args: EnterArgs) -> Result<()> {
    let already_entered = entered_session_marker().is_some();
    if already_entered && !args.new {
        bail!(
            "already inside a Manifold session; use `manifold show` to present it or `manifold enter --new` to create a separate session"
        );
    }

    let context = open_cli_control_context(args.dbus_name, args.dbus_address)?;
    let target = &context.target;
    let existing_default = resolve_directory_surface_session(target, &context.current_directory)?;
    let (session_id, set_default) = if args.new {
        let set_default = args.set_default || existing_default.is_none();
        let session_id = create_directory_surface_session(
            target,
            &context.current_directory,
            None,
            set_default,
        )?;
        let status = status_snapshot(target)?;
        eprintln!(
            "created Manifold session: {}",
            status.active_surface_session_name
        );
        (session_id, set_default)
    } else {
        match existing_default {
            Some(session_id) => (session_id, false),
            None => (
                create_directory_surface_session(target, &context.current_directory, None, true)?,
                true,
            ),
        }
    };
    remember_directory_surface_session(
        target,
        &context.current_directory,
        session_id,
        set_default,
    )?;

    let status = status_snapshot(target)?;

    let (program, command_args) = entered_argv(args.command);
    let mut command_env = vec![
        (
            MANIFOLD_SESSION_ENV.to_string(),
            OsString::from(target.session_marker_with_session(Some(session_id))),
        ),
        (
            MANIFOLDD_DBUS_DESTINATION_ENV.to_string(),
            OsString::from(&target.destination),
        ),
        (
            "MANIFOLD_SURFACE_SESSION_ID".to_string(),
            OsString::from(session_id.to_string()),
        ),
    ];
    if let Some(address) = target.address.as_deref() {
        command_env.push((
            MANIFOLDD_DBUS_ADDRESS_ENV.to_string(),
            OsString::from(address),
        ));
    }

    let mut remove_env = Vec::new();
    if args.no_capture {
        if env::var_os("WAYLAND_DISPLAY").as_deref() == Some(status.wayland_display.as_ref()) {
            remove_env.push("WAYLAND_DISPLAY");
        }
    } else {
        command_env.push((
            "WAYLAND_DISPLAY".to_string(),
            OsString::from(&status.wayland_display),
        ));
    }

    let mut entered_command_argv = vec![program.clone()];
    entered_command_argv.extend(command_args.iter().cloned());
    let status = if let Some(status) = session_runner::run_project_command_if_configured(
        context.runner.as_ref(),
        &context.current_directory,
        &entered_command_argv,
        &command_env,
        &remove_env,
    )? {
        status
    } else {
        let mut command = ProcessCommand::new(&program);
        command
            .args(&command_args)
            .envs(command_env.iter().map(|(key, value)| (key, value)));
        for key in &remove_env {
            command.env_remove(key);
        }
        command.status().context("run entered command")?
    };

    match status.code() {
        Some(code) => std::process::exit(code),
        None => bail!("entered command terminated by signal"),
    }
}

pub fn show(args: ShowArgs) -> Result<()> {
    let context = open_cli_control_context(args.dbus_name, args.dbus_address)?;
    let target = &context.target;
    let status = status_snapshot(target)?;
    if args.last_focused {
        if status.surface_sessions == 0 {
            let session_id =
                create_directory_surface_session(target, &context.current_directory, None, true)?;
            remember_directory_surface_session(
                target,
                &context.current_directory,
                session_id,
                true,
            )?;
        } else {
            restore_last_focused_surface_session(target)?;
        }
    } else {
        let (session_id, current_directory, set_default) = resolve_cli_surface_session(
            target,
            &context.current_directory,
            args.id,
            true,
            args.set_default,
            false,
        )?;
        remember_directory_surface_session(target, &current_directory, session_id, set_default)?;
        switch_surface_session(target, session_id)?;
    }

    if status.presentation == "Detached" {
        request_presentation_attach_and_focus(target)?;
    } else {
        request_presentation_window_focus(target)?;
    }
    Ok(())
}

pub fn hide(args: HideArgs) -> Result<()> {
    let context = open_cli_control_context(args.dbus_name, args.dbus_address)?;
    let target = &context.target;
    let (session_id, current_directory, set_default) = resolve_cli_surface_session(
        target,
        &context.current_directory,
        args.id,
        false,
        false,
        true,
    )?;
    remember_directory_surface_session(target, &current_directory, session_id, set_default)?;

    let status = status_snapshot(target)?;
    switch_surface_session(target, session_id)?;

    if status.presentation == "Attached" {
        let response = request_dbus_command(target, &["detach".to_string()], false)?;
        ensure_ok_response(&response)?;
    }
    Ok(())
}

pub fn list(args: ListArgs) -> Result<()> {
    let context = open_cli_control_context(args.dbus_name, args.dbus_address)?;
    if args.global {
        let list = request_surface_session_list(&context.target)?;
        print_surface_sessions(&list.sessions, None);
        return Ok(());
    }

    let list = request_directory_surface_session_list(&context.target, &context.current_directory)?;
    print_surface_sessions(&list.sessions, list.default);
    Ok(())
}

fn resolve_cli_surface_session(
    target: &ControlTarget,
    current_directory: &Path,
    query: Option<String>,
    create_if_missing: bool,
    set_default_requested: bool,
    allow_entered_fallback: bool,
) -> Result<(u64, PathBuf, bool)> {
    let existing_default = resolve_directory_surface_session(target, current_directory)?;
    let (session_id, set_default) = if let Some(query) = query {
        (
            resolve_surface_session(target, &query)?,
            set_default_requested,
        )
    } else if let Some(session_id) = existing_default {
        (session_id, false)
    } else {
        match missing_directory_surface_session_action(
            entered_surface_session_id(),
            create_if_missing,
            allow_entered_fallback,
        ) {
            MissingDirectorySurfaceSessionAction::Entered(session_id) => (session_id, true),
            MissingDirectorySurfaceSessionAction::Create => (
                create_directory_surface_session(target, current_directory, None, true)?,
                true,
            ),
            MissingDirectorySurfaceSessionAction::Active => {
                (status_snapshot(target)?.active_surface_session, false)
            }
        }
    };
    Ok((session_id, current_directory.to_path_buf(), set_default))
}

#[derive(Debug, Eq, PartialEq)]
enum MissingDirectorySurfaceSessionAction {
    Entered(u64),
    Create,
    Active,
}

const fn missing_directory_surface_session_action(
    entered_session_id: Option<u64>,
    create_if_missing: bool,
    allow_entered_fallback: bool,
) -> MissingDirectorySurfaceSessionAction {
    if allow_entered_fallback && let Some(session_id) = entered_session_id {
        return MissingDirectorySurfaceSessionAction::Entered(session_id);
    }
    if create_if_missing {
        MissingDirectorySurfaceSessionAction::Create
    } else {
        MissingDirectorySurfaceSessionAction::Active
    }
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

fn restore_last_focused_surface_session(target: &ControlTarget) -> Result<()> {
    let response = request_dbus_command(
        target,
        &["surface-session".to_string(), "last-focused".to_string()],
        false,
    )?;
    ensure_ok_response(&response)
}

fn current_directory() -> Result<PathBuf> {
    let directory = env::current_dir().context("determine current directory")?;
    Ok(directory.canonicalize().unwrap_or(directory))
}

struct CliControlContext {
    target: ControlTarget,
    current_directory: PathBuf,
    runner: Option<config::SessionRunnerSelection>,
}

fn open_cli_control_context(
    destination: Option<String>,
    address: Option<String>,
) -> Result<CliControlContext> {
    let current_directory = current_directory()?;
    let config_path = config::global_config_path();
    let runner = config::discover_session_runner(&current_directory)?;
    let target = resolve_control_target(destination, address);
    ensure_global_control_target_running(&target, &current_directory, &config_path)?;

    Ok(CliControlContext {
        target,
        current_directory,
        runner,
    })
}

fn ensure_global_control_target_running(
    target: &ControlTarget,
    current_directory: &Path,
    config_path: &Path,
) -> Result<()> {
    if !should_auto_start_global_control_target(target) {
        return Ok(());
    }
    if status_snapshot(target).is_ok() {
        return Ok(());
    }

    let (log_path, mut child) = spawn_global_server(target, current_directory, config_path)?;
    for _ in 0..1800 {
        if status_snapshot(target).is_ok() {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .context("check global Manifold server startup process")?
        {
            bail!(
                "global Manifold session server exited before becoming ready with {status}; see {}",
                log_path.display()
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "global Manifold session server did not become ready for {}; see {}",
        target.destination,
        log_path.display()
    )
}

fn should_auto_start_global_control_target(target: &ControlTarget) -> bool {
    target.destination == DEFAULT_MANIFOLD_DBUS_NAME && target.address.is_none()
}

fn spawn_global_server(
    target: &ControlTarget,
    current_directory: &Path,
    config_path: &Path,
) -> Result<(PathBuf, std::process::Child)> {
    let current_exe = env::current_exe().context("resolve current executable")?;
    let log_dir = global_server_log_dir()?;
    fs::create_dir_all(&log_dir).with_context(|| format!("create {}", log_dir.display()))?;
    let log_path = log_dir.join("global.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let log_err = log.try_clone().context("clone global server log")?;

    let mut command = ProcessCommand::new(&current_exe);
    command
        .arg("run")
        .arg("--headless")
        .arg("--no-initial-surface-session")
        .arg("--dbus-destination")
        .arg(&target.destination)
        .arg("--config")
        .arg(config_path)
        .current_dir(current_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    detach_session_server(&mut command);
    let child = command.spawn().with_context(|| {
        format!(
            "start global Manifold server {} in {}",
            current_exe.display(),
            current_directory.display()
        )
    })?;
    Ok((log_path, child))
}

fn detach_session_server(command: &mut ProcessCommand) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
}

fn global_server_log_dir() -> Result<PathBuf> {
    let root = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(env::temp_dir)
        .join("manifold")
        .join("session-server");
    Ok(root)
}

fn create_directory_surface_session(
    target: &ControlTarget,
    directory: &Path,
    name: Option<String>,
    set_default: bool,
) -> Result<u64> {
    let command = surface_session_dir_new_command(directory, name, set_default);
    let response = request_dbus_command(target, &command, false)?;
    ensure_ok_response(&response)?;
    let value = session_marker_value(response.trim(), "active")
        .context("surface-session dir-new response missing active marker")?;
    value
        .parse()
        .with_context(|| format!("parse surface session id {value:?}"))
}

fn surface_session_dir_new_command(
    directory: &Path,
    name: Option<String>,
    set_default: bool,
) -> Vec<String> {
    let mut command = vec![
        "surface-session".to_string(),
        "dir-new".to_string(),
        directory.to_string_lossy().into_owned(),
        if set_default {
            "default"
        } else {
            "keep-default"
        }
        .to_string(),
    ];
    if let Some(name) = name {
        command.push(name);
    }
    command
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

fn entered_argv(argv: Vec<OsString>) -> (OsString, Vec<OsString>) {
    let mut argv = argv.into_iter();
    let program = argv
        .next()
        .or_else(|| env::var_os("SHELL"))
        .unwrap_or_else(|| OsString::from("/bin/sh"));
    (program, argv.collect())
}

fn control_command_with_target_session(
    target: &ControlTarget,
    current_directory: &Path,
    command: Vec<String>,
    json: bool,
) -> Result<Vec<String>> {
    if json || command.first().map(String::as_str) != Some("spawn") {
        return Ok(command);
    }
    let session_id = match entered_surface_session_id() {
        Some(session_id) => Some(session_id),
        None => resolve_directory_surface_session(target, current_directory)?,
    };
    Ok(control_command_for_session(command, json, session_id))
}

fn control_command_for_session(
    command: Vec<String>,
    json: bool,
    session_id: Option<u64>,
) -> Vec<String> {
    if json || command.first().map(String::as_str) != Some("spawn") {
        return command;
    }
    let Some(session_id) = session_id else {
        return command;
    };
    let mut targeted = Vec::with_capacity(command.len() + 1);
    targeted.push("spawn-in-session".to_string());
    targeted.push(session_id.to_string());
    targeted.extend(command.into_iter().skip(1));
    targeted
}

pub fn switch(args: SwitchArgs) -> Result<()> {
    let context = open_cli_control_context(args.dbus_name, args.dbus_address)?;
    let target = &context.target;
    let list = request_surface_session_list(target)?;
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

    let selected_session = match outcome {
        SessionSwitchOutcome::Existing(selected) => {
            let response = request_dbus_command(
                target,
                &[
                    "surface-session".to_string(),
                    "switch".to_string(),
                    selected.to_string(),
                ],
                false,
            )?;
            ensure_ok_response(&response)?;
            selected
        }
        SessionSwitchOutcome::Create(name) => {
            create_directory_surface_session(target, &context.current_directory, Some(name), true)?
        }
    };
    remember_directory_surface_session(target, &context.current_directory, selected_session, true)?;

    if list.presentation == "Detached" {
        request_presentation_attach_and_focus(target)?;
    } else {
        request_presentation_window_focus(target)?;
    }

    Ok(())
}

pub fn resolve_control_target(
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
    surface_sessions: usize,
    presentation: String,
}

fn start_dbus_service(
    target: ControlTarget,
    sender: channel::Sender<DbusStateRequest>,
    runtime: tokio::runtime::Handle,
) -> Result<()> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let task = runtime.spawn(async move {
        let service_ready_tx = ready_tx.clone();
        let result = async move {
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
                .serve_at(
                    AGENT_OBJECT_PATH,
                    AgentDbus {
                        sender: sender.clone(),
                    },
                )?
                .serve_at(
                    STATUS_OBJECT_PATH,
                    StatusDbus {
                        sender: sender.clone(),
                    },
                )?
                .serve_at(COMPOSITOR_OBJECT_PATH, CompositorDbus { sender })?
                .build()
                .await?;
            service_ready_tx.send(Ok::<(), String>(())).ok();
            std::future::pending::<zbus::Result<()>>().await
        }
        .await;
        let message = match result {
            Ok(()) => "DBus control service exited".to_string(),
            Err(err) => err.to_string(),
        };
        ready_tx.send(Err(message)).ok();
    });
    drop(task);

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
    match command_name.as_str() {
        "extensions" => match args {
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
        },
        "agents" => match args {
            [subcommand] if subcommand == "list" => {
                for agent in request_agent_list(target)? {
                    print_agent(&agent);
                }
            }
            [subcommand, kind, metadata @ ..] if subcommand == "register" => {
                let id = request_agent_register(target, kind, &parse_metadata_args(metadata)?)?;
                println!("agent id={id}");
            }
            [subcommand, agent_id, status, metadata @ ..] if subcommand == "status" => {
                let changed = request_agent_update(
                    target,
                    agent_id,
                    status,
                    &parse_metadata_args(metadata)?,
                )?;
                println!("agent id={agent_id} changed={changed}");
            }
            [subcommand, agent_id, title, body @ ..] if subcommand == "notify" => {
                let body = joined_name(body).unwrap_or_default();
                let id = request_agent_notify(
                    target,
                    agent_id,
                    title,
                    "",
                    &body,
                    "waiting",
                    &DbusMetadata::new(),
                )?;
                println!("notification id={id}");
            }
            [subcommand, agent_id] if subcommand == "clear" => {
                let changed = request_agent_clear(target, agent_id)?;
                println!("agent id={agent_id} changed={changed}");
            }
            [subcommand, agent_id] if subcommand == "unregister" => {
                let changed = request_agent_unregister(target, agent_id)?;
                println!("agent id={agent_id} changed={changed}");
            }
            _ => bail!(
                "usage: agents list | agents register <kind> [key=value...] | agents status <id> <status> [key=value...] | agents notify <id> <title> [body...] | agents clear <id> | agents unregister <id>"
            ),
        },
        _ => return Ok(false),
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

fn request_presentation_attach_and_focus(target: &ControlTarget) -> Result<()> {
    let response = request_dbus_command(target, &["attach-focus".to_string()], false)?;
    ensure_ok_response(&response)
}

fn request_presentation_window_focus(target: &ControlTarget) -> Result<()> {
    let response = request_dbus_command(target, &["focus-presentation".to_string()], false)?;
    ensure_ok_response(&response)
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

fn agent_proxy<'a>(
    connection: &'a zbus::blocking::Connection,
    target: &'a ControlTarget,
) -> Result<ManifoldAgentProxyBlocking<'a>> {
    ManifoldAgentProxyBlocking::builder(connection)
        .destination(target.destination.as_str())?
        .path(AGENT_OBJECT_PATH)?
        .interface(AGENT_INTERFACE)?
        .build()
        .context("build Manifold Agent DBus proxy")
}

fn request_agent_list(target: &ControlTarget) -> Result<Vec<DbusAgent>> {
    let connection = typed_connection(target)?;
    let proxy = agent_proxy(&connection, target)?;
    proxy
        .list_agents()
        .with_context(|| format!("call Manifold Agent.ListAgents on {}", target.destination))
}

fn request_agent_register(
    target: &ControlTarget,
    kind: &str,
    metadata: &DbusMetadata,
) -> Result<String> {
    let connection = typed_connection(target)?;
    let proxy = agent_proxy(&connection, target)?;
    proxy
        .register_agent(kind.to_string(), metadata.clone())
        .with_context(|| {
            format!(
                "call Manifold Agent.RegisterAgent on {}",
                target.destination
            )
        })
}

fn request_agent_update(
    target: &ControlTarget,
    agent_id: &str,
    status: &str,
    metadata: &DbusMetadata,
) -> Result<bool> {
    let connection = typed_connection(target)?;
    let proxy = agent_proxy(&connection, target)?;
    proxy
        .update_agent(agent_id.to_string(), status.to_string(), metadata.clone())
        .with_context(|| format!("call Manifold Agent.UpdateAgent on {}", target.destination))
}

fn request_agent_notify(
    target: &ControlTarget,
    agent_id: &str,
    title: &str,
    subtitle: &str,
    body: &str,
    level: &str,
    metadata: &DbusMetadata,
) -> Result<String> {
    let connection = typed_connection(target)?;
    let proxy = agent_proxy(&connection, target)?;
    proxy
        .notify_agent(
            agent_id.to_string(),
            title.to_string(),
            subtitle.to_string(),
            body.to_string(),
            level.to_string(),
            metadata.clone(),
        )
        .with_context(|| format!("call Manifold Agent.NotifyAgent on {}", target.destination))
}

fn request_agent_clear(target: &ControlTarget, agent_id: &str) -> Result<bool> {
    let connection = typed_connection(target)?;
    let proxy = agent_proxy(&connection, target)?;
    proxy
        .clear_agent_attention(agent_id.to_string())
        .with_context(|| {
            format!(
                "call Manifold Agent.ClearAgentAttention on {}",
                target.destination
            )
        })
}

fn request_agent_unregister(target: &ControlTarget, agent_id: &str) -> Result<bool> {
    let connection = typed_connection(target)?;
    let proxy = agent_proxy(&connection, target)?;
    proxy
        .unregister_agent(agent_id.to_string())
        .with_context(|| {
            format!(
                "call Manifold Agent.UnregisterAgent on {}",
                target.destination
            )
        })
}

fn print_agent(
    (
        id,
        kind,
        status,
        sender,
        session,
        attention,
        unread,
        notifications,
        last_title,
        last_body,
        metadata,
    ): &DbusAgent,
) {
    println!(
        "agent id={} kind={} status={} sender={} session={} attention={} unread={} notifications={} last_title={} last_body={} metadata={}",
        id,
        kind,
        status,
        sender,
        session,
        attention,
        unread,
        notifications,
        last_title,
        last_body,
        format_metadata(metadata)
    );
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

fn parse_metadata_args(args: &[String]) -> Result<DbusMetadata> {
    let mut metadata = DbusMetadata::new();
    for arg in args {
        let Some((key, value)) = arg.split_once('=') else {
            bail!("metadata argument must be key=value: {arg}");
        };
        ensure!(!key.trim().is_empty(), "metadata key must not be empty");
        metadata.insert(key.trim().to_string(), value.to_string());
    }
    Ok(metadata)
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
            send_response(response, status_json_string(state));
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
                state
                    .create_and_attach_named_surface_session(Some(name))
                    .raw()
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
        DbusStateRequest::ListStatus { response } => {
            send_response(response, Ok(dbus_status_items(state.status_summaries())));
        }
        DbusStateRequest::RegisterStatusPublisher {
            role,
            metadata,
            sender_name,
            response,
        } => send_response(
            response,
            state.register_status_publisher(role, metadata, sender_name),
        ),
        DbusStateRequest::PublishStatus {
            publisher,
            channel,
            state: status_state,
            label,
            metadata,
            response,
        } => send_response(
            response,
            state.publish_status(publisher, channel, status_state, label, metadata),
        ),
        DbusStateRequest::ClearStatus {
            publisher,
            channel,
            response,
        } => send_response(response, Ok(state.clear_status(&publisher, &channel))),
        DbusStateRequest::SetPassthroughMode { enabled, response } => {
            send_response(response, Ok(state.set_shortcut_passthrough(enabled)));
        }
        DbusStateRequest::ListAgents { response } => {
            send_response(response, Ok(dbus_agents(state.agent_summaries())));
        }
        DbusStateRequest::RegisterAgent {
            kind,
            metadata,
            sender_name,
            response,
        } => send_response(response, state.register_agent(kind, metadata, sender_name)),
        DbusStateRequest::UpdateAgent {
            agent_id,
            status,
            metadata,
            sender_name,
            response,
        } => send_response(
            response,
            state.update_agent(&agent_id, status, metadata, sender_name),
        ),
        DbusStateRequest::NotifyAgent {
            agent_id,
            title,
            subtitle,
            body,
            level,
            metadata,
            sender_name,
            response,
        } => send_response(
            response,
            state.notify_agent(
                &agent_id,
                title,
                subtitle,
                body,
                level,
                metadata,
                sender_name,
            ),
        ),
        DbusStateRequest::ClearAgentAttention { agent_id, response } => {
            send_response(response, Ok(state.clear_agent_attention(&agent_id)));
        }
        DbusStateRequest::ShouldNotifyAgentDesktop { agent_id, response } => {
            send_response(response, Ok(state.should_notify_agent_desktop(&agent_id)));
        }
        DbusStateRequest::UnregisterAgent { agent_id, response } => {
            send_response(response, Ok(state.unregister_agent(&agent_id)));
        }
        DbusStateRequest::UnregisterAgentByIdentity {
            kind,
            metadata,
            response,
        } => send_response(response, state.unregister_agent_by_identity(kind, metadata)),
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
        DbusStateRequest::ReplaceRole {
            window_id,
            role,
            options,
            sender_name,
            response,
        } => send_response(
            response,
            state.replace_window_role(&window_id, role, options, sender_name),
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
        DbusStateRequest::FocusPresentationWindow { response } => {
            send_response(response, Ok(state.focus_presentation_window()));
        }
        DbusStateRequest::FocusPresentationWindowWithActivationToken {
            activation_token,
            response,
        } => send_response(
            response,
            Ok(state.focus_presentation_window_with_activation_token(&activation_token)),
        ),
        DbusStateRequest::Dismiss {
            window_id,
            response,
        } => send_response(response, state.dismiss_extension_window(&window_id)),
    }

    if let Err(err) = state.display_handle.flush_clients() {
        warn!(?err, "flush clients after DBus request failed");
    }
}

fn send_response<T>(response: DbusResponse<T>, result: Result<T>) {
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

fn dbus_agents(agents: Vec<crate::state::AgentSummary>) -> Vec<DbusAgent> {
    agents
        .into_iter()
        .map(|agent| {
            (
                agent.id,
                agent.kind,
                agent.status,
                agent.sender,
                agent.session,
                agent.attention,
                agent.unread_notifications,
                agent.notification_count,
                agent.last_notification_title.unwrap_or_default(),
                agent.last_notification_body.unwrap_or_default(),
                agent.metadata,
            )
        })
        .collect()
}

fn dbus_status_items(items: Vec<crate::state::StatusItemSummary>) -> Vec<DbusStatusItem> {
    items
        .into_iter()
        .map(|item| {
            (
                item.channel,
                item.state,
                item.label,
                item.publisher,
                item.metadata,
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
        "attach-focus" if args.is_empty() => {
            state.request_attach();
            let focused = state.focus_presentation_window();
            format!("OK attach-focus requested focused={focused}\n")
        }
        "focus-presentation" if args.is_empty() => {
            let focused = state.focus_presentation_window();
            format!("OK focus-presentation focused={focused}\n")
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
                "OK wayland_display={} wayland_client_insert_errors={} wayland_dispatch_errors={} xdg_configure_errors={} presentation={} presentation_target={} presentation_window_focused={} presentation_attach_failures={} presentation_runtime_failures={} presentation_last_error={} windows={} total_windows={} active_surface_session={} active_surface_session_name={} surface_sessions={} focused_windows={} focused_window_index={} focused_surface_id={} input_mode={} shortcut_passthrough={} managed_fullscreen={} requested_fullscreen={} focused_fullscreen={} layout_regions={} layout_placeholders={} layout_active_kind={} layout_active_rect={} resolved_layout_source={} resolved_layout_items={} resolved_layout_panes={} resolved_layout_placeholders={} left_panel_visible={} left_panel_width={} browsers={} hosted_children={} hosted_child_exits={} hosted_child_failures={} hosted_child_last_status={} frames={} client_frame_callbacks={} commits={} pending_commits={} commit_frame_count={} last_commit_batch_commits={} max_commit_batch_commits={} last_commit_to_frame_us={} max_commit_to_frame_us={} last_commit_to_submit_us={} max_commit_to_submit_us={} client_frame_callback_commit_count={} last_client_frame_callback_batch_callbacks={} max_client_frame_callback_batch_callbacks={} last_client_frame_callback_to_commit_us={} max_client_frame_callback_to_commit_us={} client_frame_deferrals={} client_deferred_frames={} last_frame_us={} max_frame_us={} last_frame_interval_us={} max_frame_interval_us={} input_frame_deferrals={} input_deferred_frames={} input_events={} keyboard_events={} pointer_motion_events={} pointer_button_events={} pointer_axis_events={} pointer_target_hits={} pointer_target_misses={} focus_clears={} focus_repairs={} window_lifecycle_refreshes={} window_lifecycle_focus_clears={} window_lifecycle_focus_repairs={} pending_input_events={} input_frame_count={} last_input_batch_events={} max_input_batch_events={} last_input_to_frame_us={} max_input_to_frame_us={} last_input_to_submit_us={} max_input_to_submit_us={} input_commit_frame_count={} input_without_commit_frame_count={} last_input_to_commit_us={} max_input_to_commit_us={} last_input_commit_to_submit_us={} max_input_commit_to_submit_us={} attach_count={} detach_count={} last_attach_latency_us={} max_attach_latency_us={} last_attach_to_frame_us={} max_attach_to_frame_us={} last_detach_latency_us={} max_detach_latency_us={} shm_formats={} egl_wl_bind={} dmabuf={} dmabuf_formats={} dmabuf_disabled={} dmabuf_attempts={} dmabuf_accepted={} dmabuf_rejected={} dmabuf_last_import={} dmabuf_last_error={} dmabuf_explicit_sync_release_points={} dmabuf_explicit_sync_blockers={} dmabuf_implicit_blockers={} dmabuf_blocker_failures={} dmabuf_last_blocker_error={} render_node={} explicit_sync={} explicit_sync_disabled={}",
                snapshot.wayland_display,
                snapshot.wayland_client_insert_errors,
                snapshot.wayland_dispatch_errors,
                snapshot.xdg_configure_errors,
                snapshot.presentation,
                snapshot.presentation_target,
                snapshot.presentation_window_focused,
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
                snapshot.shortcut_passthrough,
                snapshot.managed_fullscreen,
                snapshot.requested_fullscreen,
                snapshot.focused_fullscreen,
                snapshot.layout_regions,
                snapshot.layout_placeholders,
                snapshot.layout_active_kind,
                snapshot.layout_active_rect,
                snapshot.resolved_layout_source,
                snapshot.resolved_layout_items,
                snapshot.resolved_layout_panes,
                snapshot.resolved_layout_placeholders,
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
        "spawn-in-session" => {
            let Some((session, argv)) = args.split_first() else {
                bail!("spawn-in-session requires a session id and command");
            };
            if argv.is_empty() {
                bail!("spawn-in-session requires a command");
            }
            let session = parse_u64(session, "session id")?;
            state.spawn_hosted_app_argv_in_session(argv, SurfaceSessionId::from_raw(session))?;
            format!("OK spawned {}\n", argv.join(" "))
        }
        "plug" => {
            let (kind, path) = parse_plug_args(args)?;
            let loaded_kind = plug_wasm_module(state, &path, kind)?;
            format!("OK plug kind={} path={}\n", loaded_kind, path.display())
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
        "focus-app-id" => {
            let [app_id] = args else {
                bail!("focus-app-id requires exactly one app id");
            };
            let changed = state.focus_app_id(app_id);
            format!("OK focus-app-id app_id={app_id} changed={changed}\n")
        }
        "focus-window" => {
            let identity = parse_metadata_args(args)?;
            let changed = state.focus_window_identity(&identity);
            format!("OK focus-window changed={changed}\n")
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
        "passthrough-mode" | "passthrough" => match args {
            [subcommand] if subcommand == "get" => {
                format!(
                    "OK passthrough-mode enabled={}\n",
                    state.shortcut_passthrough()
                )
            }
            [subcommand] if subcommand == "toggle" => {
                let enabled = state.toggle_shortcut_passthrough();
                format!("OK passthrough-mode enabled={enabled} changed=true\n")
            }
            [subcommand, value] if subcommand == "set" => {
                let enabled = parse_enabled(value)?;
                let changed = state.set_shortcut_passthrough(enabled);
                format!("OK passthrough-mode enabled={enabled} changed={changed}\n")
            }
            _ => bail!("usage: passthrough-mode get|toggle|set <on|off>"),
        },
        "status-items" => match args {
            [subcommand] if subcommand == "list-json" => {
                format!("{}\n", serde_json::to_string(&state.status_summaries())?)
            }
            _ => bail!("usage: status-items list-json"),
        },
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
                    .map(super::state::SurfaceSessionId::raw);
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
            [subcommand, directory, mode, name @ ..] if subcommand == "dir-new" => {
                let set_default = match mode.as_str() {
                    "default" | "set-default" => true,
                    "keep-default" | "no-default" => false,
                    _ => bail!("dir-new mode must be default or keep-default"),
                };
                let directory = Path::new(directory).to_path_buf();
                let session = state.create_and_attach_named_surface_session_in_directory(
                    joined_name(name),
                    directory.clone(),
                );
                let changed = state.remember_directory_surface_session(
                    directory.clone(),
                    session,
                    set_default,
                );
                ensure!(changed, "unknown surface session {}", session.raw());
                format!(
                    "OK surface-session active={} created=true directory={} default={set_default}\n",
                    session.raw(),
                    directory.display()
                )
            }
            [subcommand] if subcommand == "next" => {
                let changed =
                    WorkspaceCommand::Surface(SurfaceCommand::SwitchActiveSessionNext).apply(state);
                format!(
                    "OK surface-session active={} changed={changed}\n",
                    state.active_surface_session().raw()
                )
            }
            [subcommand] if subcommand == "last-focused" => {
                let changed = state.restore_last_interacted_surface_session();
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
                "usage: surface-session list-json|dir-list-json <dir>|dir-get <dir>|dir-set <dir> <id> [default|keep-default]|next|last-focused|new|switch <query>"
            ),
        },
        "browser" => browser::handle_control(args, state)?,
        "harness" => match args {
            [subcommand, name] if subcommand == "signal" => {
                state.signal_test_harness(name.clone())?;
                format!("OK harness signal {name}\n")
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
                format!("OK debug dump {manifest}\n")
            }
            [subcommand, dir, selectors] if subcommand == "dump" => {
                let manifest = debug_dump::dump_bundle(
                    state,
                    Path::new(dir),
                    DumpOptions::parse(Some(selectors))?,
                )?;
                format!("OK debug dump {manifest}\n")
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WasmPlugKind {
    Auto,
    FastKeyHandler,
    FastMapHook,
}

impl WasmPlugKind {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "fast-key-handler" | "key" | "key-handler" => Ok(Self::FastKeyHandler),
            "fast-map-hook" | "map" | "map-hook" => Ok(Self::FastMapHook),
            _ => bail!("plug kind must be auto, fast-key-handler, or fast-map-hook"),
        }
    }
}

fn parse_plug_args(args: &[String]) -> Result<(WasmPlugKind, PathBuf)> {
    let mut kind = WasmPlugKind::Auto;
    let mut path = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--kind" | "-k" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    bail!("plug --kind requires a value");
                };
                kind = WasmPlugKind::parse(value)?;
            }
            arg if arg.starts_with("--kind=") => {
                let value = arg.trim_start_matches("--kind=");
                kind = WasmPlugKind::parse(value)?;
            }
            arg if arg.starts_with('-') => {
                bail!("unknown plug option {arg}");
            }
            arg => {
                ensure!(path.is_none(), "plug accepts exactly one WASM module path");
                path = Some(PathBuf::from(arg));
            }
        }
        index += 1;
    }

    let path = path.context("usage: plug [--kind auto|fast-key-handler|fast-map-hook] <path>")?;
    Ok((kind, path))
}

fn plug_wasm_module(state: &mut Manifold, path: &Path, kind: WasmPlugKind) -> Result<&'static str> {
    ensure!(
        path.exists(),
        "WASM module does not exist: {}",
        path.display()
    );
    match kind {
        WasmPlugKind::FastKeyHandler => {
            state.replace_wasm_key_handler(path)?;
            Ok("fast-key-handler")
        }
        WasmPlugKind::FastMapHook => {
            state.replace_wasm_map_hook(path)?;
            Ok("fast-map-hook")
        }
        WasmPlugKind::Auto => {
            let key_error = match state.replace_wasm_key_handler(path) {
                Ok(()) => return Ok("fast-key-handler"),
                Err(err) => err,
            };
            match state.replace_wasm_map_hook(path) {
                Ok(()) => Ok("fast-map-hook"),
                Err(map_error) => Err(anyhow::anyhow!(
                    "could not plug {} as fast-key-handler ({key_error}) or fast-map-hook ({map_error})",
                    path.display()
                )),
            }
        }
    }
}

#[derive(serde::Serialize)]
struct StatusSnapshot {
    wayland_display: String,
    wayland_client_insert_errors: u64,
    wayland_dispatch_errors: u64,
    xdg_configure_errors: u64,
    presentation: &'static str,
    presentation_target: &'static str,
    presentation_window_focused: bool,
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
    shortcut_passthrough: bool,
    managed_fullscreen: bool,
    requested_fullscreen: usize,
    focused_fullscreen: bool,
    layout_regions: usize,
    layout_placeholders: usize,
    layout_active_kind: &'static str,
    layout_active_rect: String,
    resolved_layout_source: &'static str,
    resolved_layout_items: usize,
    resolved_layout_panes: usize,
    resolved_layout_placeholders: usize,
    left_panel_visible: bool,
    left_panel_width: i32,
    browsers: usize,
    agents: Vec<crate::state::AgentSummary>,
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
                    .map(super::winit_presenter::PresenterBackend::target_name)
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
            presentation_window_focused: state.presentation_window_focused(),
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
            shortcut_passthrough: state.shortcut_passthrough(),
            managed_fullscreen: state.managed_fullscreen(),
            requested_fullscreen: state.requested_fullscreen_count(),
            focused_fullscreen: state.focused_window_fullscreen(),
            layout_regions: state.resolved_layout_item_count(),
            layout_placeholders: state.resolved_layout_placeholder_count(),
            layout_active_kind: state.resolved_layout_active_kind(),
            layout_active_rect,
            resolved_layout_source: state.resolved_layout_source_name(),
            resolved_layout_items: state.resolved_layout_item_count(),
            resolved_layout_panes: state.resolved_layout_pane_count(),
            resolved_layout_placeholders: state.resolved_layout_placeholder_count(),
            left_panel_visible: state.left_panel_reserved_width() > 0,
            left_panel_width: state.left_panel_reserved_width(),
            browsers: state.browsers.len(),
            agents: state.agent_summaries(),
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
                .map_or_else(|| "none".to_string(), sanitize_ctl_field),
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

fn parse_enabled(value: &str) -> Result<bool> {
    match value {
        "on" | "true" | "yes" | "1" | "enabled" | "enable" => Ok(true),
        "off" | "false" | "no" | "0" | "disabled" | "disable" => Ok(false),
        _ => bail!("value must be on or off"),
    }
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
#[allow(
    clippy::unwrap_used,
    reason = "unit tests use direct parse assertions where panic output is useful"
)]
mod tests {
    use super::{
        ControlTarget, MissingDirectorySurfaceSessionAction, SessionSwitchOutcome, WasmPlugKind,
        control_command_for_session, format_session_picker_row,
        missing_directory_surface_session_action, parse_plug_args, parse_skim_session_output,
        resolve_initial_query, session_marker_value, surface_session_dir_new_command,
    };
    use crate::state::SurfaceSessionSummary;
    use std::path::Path;

    fn session(id: u64, name: &str) -> SurfaceSessionSummary {
        SurfaceSessionSummary {
            id,
            name: name.to_string(),
            active: false,
            windows: 0,
            working_directory: None,
        }
    }

    #[test]
    fn picker_row_starts_with_machine_readable_session_id() {
        let row = format_session_picker_row(&SurfaceSessionSummary {
            id: 7,
            name: "notes".to_string(),
            active: true,
            windows: 2,
            working_directory: None,
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
    fn plug_args_default_to_auto_kind() {
        let (kind, path) = parse_plug_args(&["module.wasm".to_string()]).unwrap();

        assert_eq!(kind, WasmPlugKind::Auto);
        assert_eq!(path, std::path::PathBuf::from("module.wasm"));
    }

    #[test]
    fn plug_args_accept_kind_option() {
        let (kind, path) = parse_plug_args(&[
            "--kind".to_string(),
            "fast-map-hook".to_string(),
            "layout.wasm".to_string(),
        ])
        .unwrap();

        assert_eq!(kind, WasmPlugKind::FastMapHook);
        assert_eq!(path, std::path::PathBuf::from("layout.wasm"));
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

    #[test]
    fn show_missing_directory_creates_even_from_entered_session() {
        assert_eq!(
            missing_directory_surface_session_action(Some(42), true, false),
            MissingDirectorySurfaceSessionAction::Create
        );
    }

    #[test]
    fn entered_fallback_remains_available_for_callers_that_request_it() {
        assert_eq!(
            missing_directory_surface_session_action(Some(42), true, true),
            MissingDirectorySurfaceSessionAction::Entered(42)
        );
    }

    #[test]
    fn entered_spawn_command_targets_entered_session() {
        let command = control_command_for_session(
            vec!["spawn".to_string(), "foot".to_string()],
            false,
            Some(42),
        );

        assert_eq!(
            command,
            vec!["spawn-in-session", "42", "foot"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn json_spawn_command_is_not_rewritten() {
        let command = control_command_for_session(
            vec!["spawn".to_string(), "foot".to_string()],
            true,
            Some(42),
        );

        assert_eq!(command, vec!["spawn".to_string(), "foot".to_string()]);
    }

    #[test]
    fn dir_new_command_carries_client_directory_and_default_mode() {
        let command = surface_session_dir_new_command(Path::new("/tmp/project/subdir"), None, true);

        assert_eq!(
            command,
            vec![
                "surface-session",
                "dir-new",
                "/tmp/project/subdir",
                "default"
            ]
        );
    }

    #[test]
    fn dir_new_command_carries_name_without_setting_default() {
        let command = surface_session_dir_new_command(
            Path::new("/tmp/project/subdir"),
            Some("scratch".to_string()),
            false,
        );

        assert_eq!(
            command,
            vec![
                "surface-session",
                "dir-new",
                "/tmp/project/subdir",
                "keep-default",
                "scratch"
            ]
        );
    }
}
