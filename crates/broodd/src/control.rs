#![allow(
    clippy::expect_used,
    clippy::let_underscore_must_use,
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::option_if_let_else,
    clippy::too_many_arguments,
    clippy::unnecessary_wraps,
    reason = "tracked in issues.md: DBus waits still cross the compositor actor boundary"
)]
// TODO(lint-structure): separate the DBus proxy ABI from compositor state dispatch.

use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    future::Future,
    io::Write,
    os::fd::AsFd,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
pub use brood_cli::control::{
    ControlTarget, request_dbus_command, resolve_control_target, session_marker_value,
};
use smithay::{
    backend::{
        allocator::Buffer,
        input::{ButtonState, KeyState},
    },
    reexports::calloop::{
        EventLoop, Interest, Mode, PostAction,
        channel::{self, Event},
        generic::Generic,
    },
    utils::Point,
};
use tracing::{info, warn};
use zbus::{fdo, message::Header, object_server::SignalEmitter, proxy, zvariant};

use crate::{
    BROOD_SESSION_ENV, BROODD_DBUS_ADDRESS_ENV, BROODD_DBUS_DESTINATION_ENV, CommandOutcome,
    CtlArgs, DEFAULT_BROOD_DBUS_NAME, EnterArgs, HideArgs, ListArgs, PlugArgs, ShowArgs,
    ShowTarget, SwitchArgs, browser,
    command::{SurfaceCommand, WorkspaceCommand},
    config,
    debug_dump::{self, DumpOptions},
    session_runner,
    state::{
        AgentNotification, Brood, FocusDirection, HostedChildStatus, HostedChildToken,
        SurfaceSessionId, SurfaceSessionSummary,
    },
};

const CONTROL_OBJECT_PATH: &str = "/org/brood/Session";
const MANAGER_OBJECT_PATH: &str = "/org/brood/Manager";
const EXTENSION_OBJECT_PATH: &str = "/org/brood/Extension";
const PLUGIN_OBJECT_PATH: &str = "/org/brood/Plugin";
const COMPOSITOR_OBJECT_PATH: &str = "/org/brood/Compositor";
const AGENT_OBJECT_PATH: &str = "/org/brood/Agent";
const STATUS_OBJECT_PATH: &str = "/org/brood/Status";
const MANAGER_INTERFACE: &str = "org.brood.Manager";
const EXTENSION_INTERFACE: &str = "org.brood.Extension";
const PLUGIN_INTERFACE: &str = "org.brood.Plugin";
const AGENT_INTERFACE: &str = "org.brood.Agent";
const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(15);

type DbusMetadata = BTreeMap<String, String>;
type DbusSurfaceSession = (u64, String, bool, u64);
type DbusPresentationWindow = (u64, u64, String);
type DbusExtension = (String, String, String, DbusMetadata);
type DbusGrant = (String, String, String, String, bool, DbusMetadata);
type DbusStatusItem = (String, String, String, String, DbusMetadata);
type DbusPluginActor = (String, String, u64, u64, u64, u64, String, String);
type DbusPluginMailbox = (String, u64, u64, u64);
type DbusDmabufFrame = (
    u64,
    String,
    u32,
    u32,
    u32,
    u32,
    u64,
    u32,
    u32,
    bool,
    zvariant::OwnedFd,
);
type DbusFastPath = (
    u64,
    u64,
    zvariant::OwnedFd,
    zvariant::OwnedFd,
    zvariant::OwnedFd,
    zvariant::OwnedFd,
    zvariant::OwnedFd,
    zvariant::OwnedFd,
    zvariant::OwnedFd,
    zvariant::OwnedFd,
    zvariant::OwnedFd,
    zvariant::OwnedFd,
);
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

type DbusResponse<T> = tokio::sync::oneshot::Sender<std::result::Result<T, String>>;
type ControlResponse = DbusResponse<String>;
type BoolResponse = DbusResponse<bool>;
type U64Response = DbusResponse<u64>;
type SessionListResponse = DbusResponse<Vec<DbusSurfaceSession>>;
type PresentationWindowListResponse = DbusResponse<Vec<DbusPresentationWindow>>;
type ExtensionListResponse = DbusResponse<Vec<DbusExtension>>;
type GrantListResponse = DbusResponse<Vec<DbusGrant>>;
type StatusListResponse = DbusResponse<Vec<DbusStatusItem>>;
type PluginActorListResponse = DbusResponse<Vec<DbusPluginActor>>;
type PluginMailboxListResponse = DbusResponse<Vec<DbusPluginMailbox>>;
type AgentListResponse = DbusResponse<Vec<DbusAgent>>;
type OptionalControlResponse = DbusResponse<Option<String>>;
type DmabufFrameResponse = DbusResponse<DbusDmabufFrame>;
type FastPathResponse = DbusResponse<DbusFastPath>;

enum DbusStateRequest {
    ControlCommand {
        command: Vec<String>,
        json: bool,
        response: ControlResponse,
    },
    CurrentStatus {
        response: ControlResponse,
    },
    LatestDmabufFrame {
        response: DmabufFrameResponse,
    },
    OpenPresentationFastPath {
        response: FastPathResponse,
    },
    DmabufResource {
        resource_id: u64,
        response: DmabufFrameResponse,
    },
    ListSurfaceSessions {
        response: SessionListResponse,
    },
    ListPresentationWindows {
        response: PresentationWindowListResponse,
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
    ListPluginActors {
        response: PluginActorListResponse,
    },
    ListPluginMailboxes {
        actor_id: String,
        response: PluginMailboxListResponse,
    },
    SendPluginActorMessage {
        actor_id: String,
        message: String,
        sender_name: String,
        response: BoolResponse,
    },
    SendPluginActorSlotMessage {
        actor_id: String,
        mailbox_slot: String,
        message: String,
        sender_name: String,
        response: BoolResponse,
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
struct PluginDbus {
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

#[zbus::interface(name = "org.brood.Session.Control")]
impl ControlDbus {
    async fn command(&self, argv: Vec<String>, json: bool) -> fdo::Result<String> {
        request_state(&self.sender, |response| DbusStateRequest::ControlCommand {
            command: argv,
            json,
            response,
        })
        .await
    }

    async fn latest_dmabuf_frame(&self) -> fdo::Result<DbusDmabufFrame> {
        request_state(&self.sender, |response| {
            DbusStateRequest::LatestDmabufFrame { response }
        })
        .await
    }

    async fn open_presentation_fast_path(&self) -> fdo::Result<DbusFastPath> {
        request_state(&self.sender, |response| {
            DbusStateRequest::OpenPresentationFastPath { response }
        })
        .await
    }

    async fn dmabuf_resource(&self, resource_id: u64) -> fdo::Result<DbusDmabufFrame> {
        request_state(&self.sender, |response| DbusStateRequest::DmabufResource {
            resource_id,
            response,
        })
        .await
    }
}

#[zbus::interface(name = "org.brood.Manager")]
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

#[zbus::interface(name = "org.brood.Session")]
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

    async fn list_presentation_windows(&self) -> fdo::Result<Vec<DbusPresentationWindow>> {
        request_state(&self.sender, |response| {
            DbusStateRequest::ListPresentationWindows { response }
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

#[zbus::interface(name = "org.brood.Extension")]
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

#[zbus::interface(name = "org.brood.Plugin")]
impl PluginDbus {
    async fn list_actors(&self) -> fdo::Result<Vec<DbusPluginActor>> {
        request_state(&self.sender, |response| {
            DbusStateRequest::ListPluginActors { response }
        })
        .await
    }

    async fn list_mailboxes(&self, actor_id: String) -> fdo::Result<Vec<DbusPluginMailbox>> {
        request_state(&self.sender, |response| {
            DbusStateRequest::ListPluginMailboxes { actor_id, response }
        })
        .await
    }

    async fn send_message(
        &self,
        actor_id: String,
        message: String,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<bool> {
        let sender_name = header_sender(&header);
        request_state(&self.sender, |response| {
            DbusStateRequest::SendPluginActorMessage {
                actor_id,
                message,
                sender_name,
                response,
            }
        })
        .await
    }

    async fn send_message_to_slot(
        &self,
        actor_id: String,
        mailbox_slot: String,
        message: String,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<bool> {
        let sender_name = header_sender(&header);
        request_state(&self.sender, |response| {
            DbusStateRequest::SendPluginActorSlotMessage {
                actor_id,
                mailbox_slot,
                message,
                sender_name,
                response,
            }
        })
        .await
    }
}

#[zbus::interface(name = "org.brood.Agent")]
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

#[zbus::interface(name = "org.brood.Status")]
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

#[zbus::interface(name = "org.brood.Compositor")]
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
            fdo::Error::Failed(format!("Brood session loop is not running: {err}"))
        })?;
        tokio::time::timeout(CONTROL_REPLY_TIMEOUT, response_rx)
            .await
            .map_err(|err| {
                fdo::Error::Failed(format!(
                    "timed out waiting for Brood control response: {err}"
                ))
            })?
            .map_err(|err| {
                fdo::Error::Failed(format!("Brood control response was dropped: {err}"))
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
    interface = "org.brood.Session.Control",
    default_path = "/org/brood/Session"
)]
trait BroodControl {
    fn command(&self, argv: Vec<String>, json: bool) -> zbus::Result<String>;
}

#[proxy(interface = "org.brood.Manager", default_path = "/org/brood/Manager")]
trait BroodManager {
    fn list_sessions(&self) -> zbus::Result<Vec<DbusSurfaceSession>>;
    fn active_session(&self) -> zbus::Result<u64>;
    fn resolve_session(&self, session: String) -> zbus::Result<u64>;
    fn list_extensions(&self) -> zbus::Result<Vec<DbusExtension>>;
}

#[proxy(interface = "org.brood.Session", default_path = "/org/brood/Session")]
trait BroodSession {
    fn current_status(&self) -> zbus::Result<String>;
    fn list_surface_sessions(&self) -> zbus::Result<Vec<DbusSurfaceSession>>;
    fn list_presentation_windows(&self) -> zbus::Result<Vec<DbusPresentationWindow>>;
    fn activate_surface_session(&self, id: u64) -> zbus::Result<bool>;
    fn create_surface_session(&self, name: String) -> zbus::Result<u64>;
    fn rename_surface_session(&self, id: u64, name: String) -> zbus::Result<bool>;
    fn close_surface_session(&self, id: u64) -> zbus::Result<bool>;
}

#[proxy(
    interface = "org.brood.Extension",
    default_path = "/org/brood/Extension"
)]
trait BroodExtension {
    fn register_extension(&self, role: String, metadata: DbusMetadata) -> zbus::Result<String>;
    fn unregister_extension(&self, extension_id: String) -> zbus::Result<bool>;
    fn list_grants(&self, extension_id: String) -> zbus::Result<Vec<DbusGrant>>;
    fn release_grant(&self, grant_id: String) -> zbus::Result<bool>;
}

#[proxy(interface = "org.brood.Plugin", default_path = "/org/brood/Plugin")]
trait BroodPlugin {
    fn list_actors(&self) -> zbus::Result<Vec<DbusPluginActor>>;
    fn list_mailboxes(&self, actor_id: String) -> zbus::Result<Vec<DbusPluginMailbox>>;
    fn send_message(&self, actor_id: String, message: String) -> zbus::Result<bool>;
    fn send_message_to_slot(
        &self,
        actor_id: String,
        mailbox_slot: String,
        message: String,
    ) -> zbus::Result<bool>;
}

#[proxy(interface = "org.brood.Agent", default_path = "/org/brood/Agent")]
trait BroodAgent {
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

#[proxy(interface = "org.brood.Status", default_path = "/org/brood/Status")]
trait BroodStatus {
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
}

#[proxy(
    interface = "org.brood.Compositor",
    default_path = "/org/brood/Compositor"
)]
trait BroodCompositor {
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
    event_loop: &mut EventLoop<'static, Brood>,
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
    let mut command = vec!["plug".to_string(), "--kind".to_string(), args.kind];
    if let Some(instance_id) = args.id {
        command.push("--id".to_string());
        command.push(instance_id);
    }
    command.push(path.display().to_string());
    let response = request_dbus_command(&context.target, &command, false)?;
    ensure_ok_response(&response)?;
    print!("{response}");
    Ok(())
}

pub fn enter(args: EnterArgs) -> Result<CommandOutcome> {
    let already_entered = entered_session_marker().is_some();
    if already_entered && !args.new {
        bail!(
            "already inside a Brood session; use `brood show` to present it or `brood enter --new` to create a separate session"
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
            "created Brood session: {}",
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
            BROOD_SESSION_ENV.to_string(),
            OsString::from(target.session_marker_with_session(Some(session_id))),
        ),
        (
            BROODD_DBUS_DESTINATION_ENV.to_string(),
            OsString::from(&target.destination),
        ),
        (
            "BROOD_SURFACE_SESSION_ID".to_string(),
            OsString::from(session_id.to_string()),
        ),
    ];
    if let Some(address) = target.address.as_deref() {
        command_env.push((BROODD_DBUS_ADDRESS_ENV.to_string(), OsString::from(address)));
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

    entered_command_outcome(status)
}

fn entered_command_outcome(status: std::process::ExitStatus) -> Result<CommandOutcome> {
    match status.code() {
        Some(code) => Ok(CommandOutcome::Exit(code)),
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
        let allow_entered_fallback = args.id.is_none() && args.target != Some(ShowTarget::Here);
        let resolution = resolve_cli_surface_session(
            target,
            &context.current_directory,
            args.id,
            true,
            args.set_default,
            allow_entered_fallback,
        )?;
        if resolution.remember_directory {
            remember_directory_surface_session(
                target,
                &resolution.current_directory,
                resolution.session_id,
                resolution.set_default,
            )?;
        }
        switch_surface_session(target, resolution.session_id)?;
    }

    if status.presentation == "Detached" {
        brood_cli::control::spawn_client(target)?;
    } else {
        request_presentation_window_focus(target)?;
    }
    Ok(())
}

pub fn hide(args: HideArgs) -> Result<()> {
    let context = open_cli_control_context(args.dbus_name, args.dbus_address)?;
    let target = &context.target;
    let resolution = resolve_cli_surface_session(
        target,
        &context.current_directory,
        args.id,
        false,
        false,
        true,
    )?;
    if resolution.remember_directory {
        remember_directory_surface_session(
            target,
            &resolution.current_directory,
            resolution.session_id,
            resolution.set_default,
        )?;
    }

    let status = status_snapshot(target)?;
    switch_surface_session(target, resolution.session_id)?;

    if status.presentation == "Attached" {
        let response = request_dbus_command(target, &["close-frontend".to_string()], false)?;
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

struct CliSurfaceSessionResolution {
    session_id: u64,
    current_directory: PathBuf,
    set_default: bool,
    remember_directory: bool,
}

fn resolve_cli_surface_session(
    target: &ControlTarget,
    current_directory: &Path,
    query: Option<String>,
    create_if_missing: bool,
    set_default_requested: bool,
    allow_entered_fallback: bool,
) -> Result<CliSurfaceSessionResolution> {
    let existing_default = resolve_directory_surface_session(target, current_directory)?;
    let (session_id, set_default, remember_directory) = if let Some(query) = query {
        let session_id = resolve_surface_session(target, &query)?;
        (session_id, set_default_requested, true)
    } else {
        match choose_cli_surface_session(
            existing_default,
            entered_surface_session_id(),
            create_if_missing,
            allow_entered_fallback,
        ) {
            MissingDirectorySurfaceSessionAction::Entered(session_id) => (session_id, false, false),
            MissingDirectorySurfaceSessionAction::ExistingDefault(session_id) => {
                (session_id, false, true)
            }
            MissingDirectorySurfaceSessionAction::Create => (
                create_directory_surface_session(target, current_directory, None, true)?,
                true,
                true,
            ),
            MissingDirectorySurfaceSessionAction::Active => {
                (status_snapshot(target)?.active_surface_session, false, true)
            }
        }
    };
    Ok(CliSurfaceSessionResolution {
        session_id,
        current_directory: current_directory.to_path_buf(),
        set_default,
        remember_directory,
    })
}

#[derive(Debug, Eq, PartialEq)]
enum MissingDirectorySurfaceSessionAction {
    Entered(u64),
    ExistingDefault(u64),
    Create,
    Active,
}

const fn choose_cli_surface_session(
    existing_default: Option<u64>,
    entered_session_id: Option<u64>,
    create_if_missing: bool,
    allow_entered_fallback: bool,
) -> MissingDirectorySurfaceSessionAction {
    if allow_entered_fallback && let Some(session_id) = entered_session_id {
        return MissingDirectorySurfaceSessionAction::Entered(session_id);
    }
    if let Some(session_id) = existing_default {
        return MissingDirectorySurfaceSessionAction::ExistingDefault(session_id);
    }
    missing_directory_surface_session_action(entered_session_id, create_if_missing, false)
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
    if target.destination != DEFAULT_BROOD_DBUS_NAME || target.address.is_some() {
        return Ok(());
    }
    let mut launch = brood_cli::control::ServerLaunch::discover()?;
    launch.current_directory = current_directory.to_path_buf();
    launch.config = Some(config_path.to_path_buf());
    brood_cli::control::connect_or_spawn(target, &launch)?;
    Ok(())
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
    serde_json::from_str(response.trim()).context("decode Brood status-json")
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
    env::var(BROOD_SESSION_ENV).ok().or_else(|| {
        env::var("BROOD_SURFACE_SESSION_ID")
            .ok()
            .map(|session_id| format!("surface-session-id={session_id}"))
    })
}

fn entered_surface_session_id() -> Option<u64> {
    env::var("BROOD_SURFACE_SESSION_ID")
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
        brood_cli::control::spawn_client(target)?;
    } else {
        request_presentation_window_focus(target)?;
    }

    Ok(())
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
                    PLUGIN_OBJECT_PATH,
                    PluginDbus {
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
        "actors" => match args {
            [subcommand] if subcommand == "list" => {
                for actor in request_plugin_actor_list(target)? {
                    print_plugin_actor(&actor);
                }
            }
            [subcommand, actor_id] if subcommand == "mailboxes" => {
                for mailbox in request_plugin_mailbox_list(target, actor_id)? {
                    print_plugin_mailbox(actor_id, &mailbox);
                }
            }
            [subcommand, actor_id, message @ ..] if subcommand == "send" => {
                let queued = request_plugin_actor_send(target, actor_id, &message.join(" "))?;
                println!("actor id={actor_id} queued={queued}");
            }
            [subcommand, actor_id, mailbox_slot, message @ ..] if subcommand == "send-slot" => {
                let queued = request_plugin_actor_send_to_slot(
                    target,
                    actor_id,
                    mailbox_slot,
                    &message.join(" "),
                )?;
                println!("actor id={actor_id} mailbox_slot={mailbox_slot} queued={queued}");
            }
            _ => bail!(
                "usage: actors list | actors mailboxes <actor-id> | actors send <actor-id> [message...] | actors send-slot <actor-id> <slot> [message...]"
            ),
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

fn request_presentation_window_focus(target: &ControlTarget) -> Result<()> {
    let response = request_dbus_command(target, &["focus-presentation".to_string()], false)?;
    ensure_ok_response(&response)
}

fn request_extension_list(target: &ControlTarget) -> Result<Vec<DbusExtension>> {
    let connection = typed_connection(target)?;
    let proxy = BroodManagerProxyBlocking::builder(&connection)
        .destination(target.destination.as_str())?
        .path(MANAGER_OBJECT_PATH)?
        .interface(MANAGER_INTERFACE)?
        .build()
        .context("build Brood Manager DBus proxy")?;
    proxy.list_extensions().with_context(|| {
        format!(
            "call Brood Manager.ListExtensions on {}",
            target.destination
        )
    })
}

fn request_grant_list(target: &ControlTarget, extension_id: &str) -> Result<Vec<DbusGrant>> {
    let connection = typed_connection(target)?;
    let proxy = BroodExtensionProxyBlocking::builder(&connection)
        .destination(target.destination.as_str())?
        .path(EXTENSION_OBJECT_PATH)?
        .interface(EXTENSION_INTERFACE)?
        .build()
        .context("build Brood Extension DBus proxy")?;
    proxy
        .list_grants(extension_id.to_string())
        .with_context(|| format!("call Brood Extension.ListGrants on {}", target.destination))
}

fn plugin_proxy<'a>(
    connection: &'a zbus::blocking::Connection,
    target: &'a ControlTarget,
) -> Result<BroodPluginProxyBlocking<'a>> {
    BroodPluginProxyBlocking::builder(connection)
        .destination(target.destination.as_str())?
        .path(PLUGIN_OBJECT_PATH)?
        .interface(PLUGIN_INTERFACE)?
        .build()
        .context("build Brood Plugin DBus proxy")
}

fn request_plugin_actor_list(target: &ControlTarget) -> Result<Vec<DbusPluginActor>> {
    let connection = typed_connection(target)?;
    let proxy = plugin_proxy(&connection, target)?;
    proxy
        .list_actors()
        .with_context(|| format!("call Brood Plugin.ListActors on {}", target.destination))
}

fn request_plugin_mailbox_list(
    target: &ControlTarget,
    actor_id: &str,
) -> Result<Vec<DbusPluginMailbox>> {
    let connection = typed_connection(target)?;
    let proxy = plugin_proxy(&connection, target)?;
    proxy
        .list_mailboxes(actor_id.to_string())
        .with_context(|| format!("call Brood Plugin.ListMailboxes on {}", target.destination))
}

fn request_plugin_actor_send(
    target: &ControlTarget,
    actor_id: &str,
    message: &str,
) -> Result<bool> {
    let connection = typed_connection(target)?;
    let proxy = plugin_proxy(&connection, target)?;
    proxy
        .send_message(actor_id.to_string(), message.to_string())
        .with_context(|| format!("call Brood Plugin.SendMessage on {}", target.destination))
}

fn request_plugin_actor_send_to_slot(
    target: &ControlTarget,
    actor_id: &str,
    mailbox_slot: &str,
    message: &str,
) -> Result<bool> {
    let connection = typed_connection(target)?;
    let proxy = plugin_proxy(&connection, target)?;
    proxy
        .send_message_to_slot(
            actor_id.to_string(),
            mailbox_slot.to_string(),
            message.to_string(),
        )
        .with_context(|| {
            format!(
                "call Brood Plugin.SendMessageToSlot on {}",
                target.destination
            )
        })
}

fn agent_proxy<'a>(
    connection: &'a zbus::blocking::Connection,
    target: &'a ControlTarget,
) -> Result<BroodAgentProxyBlocking<'a>> {
    BroodAgentProxyBlocking::builder(connection)
        .destination(target.destination.as_str())?
        .path(AGENT_OBJECT_PATH)?
        .interface(AGENT_INTERFACE)?
        .build()
        .context("build Brood Agent DBus proxy")
}

fn request_agent_list(target: &ControlTarget) -> Result<Vec<DbusAgent>> {
    let connection = typed_connection(target)?;
    let proxy = agent_proxy(&connection, target)?;
    proxy
        .list_agents()
        .with_context(|| format!("call Brood Agent.ListAgents on {}", target.destination))
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
        .with_context(|| format!("call Brood Agent.RegisterAgent on {}", target.destination))
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
        .with_context(|| format!("call Brood Agent.UpdateAgent on {}", target.destination))
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
        .with_context(|| format!("call Brood Agent.NotifyAgent on {}", target.destination))
}

fn request_agent_clear(target: &ControlTarget, agent_id: &str) -> Result<bool> {
    let connection = typed_connection(target)?;
    let proxy = agent_proxy(&connection, target)?;
    proxy
        .clear_agent_attention(agent_id.to_string())
        .with_context(|| {
            format!(
                "call Brood Agent.ClearAgentAttention on {}",
                target.destination
            )
        })
}

fn request_agent_unregister(target: &ControlTarget, agent_id: &str) -> Result<bool> {
    let connection = typed_connection(target)?;
    let proxy = agent_proxy(&connection, target)?;
    proxy
        .unregister_agent(agent_id.to_string())
        .with_context(|| format!("call Brood Agent.UnregisterAgent on {}", target.destination))
}

fn print_plugin_actor(
    (
        id,
        kind,
        enqueued_messages,
        dropped_messages,
        mailbox_capacity,
        published_frames,
        latest_frame,
        mailbox_slots,
    ): &DbusPluginActor,
) {
    println!(
        "actor id={id} kind={kind} enqueued_messages={enqueued_messages} dropped_messages={dropped_messages} mailbox_capacity={mailbox_capacity} published_frames={published_frames} latest_frame={latest_frame} mailbox_slots={mailbox_slots}"
    );
}

fn print_plugin_mailbox(
    actor_id: &str,
    (slot, enqueued_messages, dropped_messages, capacity): &DbusPluginMailbox,
) {
    println!(
        "actor id={actor_id} mailbox_slot={slot} enqueued_messages={enqueued_messages} dropped_messages={dropped_messages} capacity={capacity}"
    );
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

pub fn execute_json_command(input: &str, state: &mut Brood) -> Result<String> {
    let tokens = parse_command_tokens(input.trim())?;
    execute_tokens(&tokens, state)
}

fn execute_dbus_request(command: Vec<String>, json: bool, state: &mut Brood) -> Result<String> {
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

fn execute_state_request(request: DbusStateRequest, state: &mut Brood) {
    match request {
        DbusStateRequest::ControlCommand {
            command,
            json,
            response,
        } => send_response(response, execute_dbus_request(command, json, state)),
        DbusStateRequest::CurrentStatus { response } => {
            send_response(response, status_json_string(state));
        }
        DbusStateRequest::LatestDmabufFrame { response } => {
            send_response(response, latest_dmabuf_frame(state));
        }
        DbusStateRequest::OpenPresentationFastPath { response } => {
            send_response(response, open_presentation_fast_path(state));
        }
        DbusStateRequest::DmabufResource {
            resource_id,
            response,
        } => send_response(response, dmabuf_resource(state, resource_id)),
        DbusStateRequest::ListSurfaceSessions { response } => {
            state.refresh_window_state();
            let sessions = dbus_surface_sessions(state.surface_session_summaries());
            send_response(response, Ok(sessions));
        }
        DbusStateRequest::ListPresentationWindows { response } => {
            state.refresh_window_state();
            let windows = state
                .presentation_window_summaries()
                .into_iter()
                .map(|window| (window.pane, window.session, window.app_id))
                .collect();
            send_response(response, Ok(windows));
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
            let result = if let Some(name) = normalize_session_name(&name) {
                state
                    .create_and_attach_named_surface_session(Some(name))
                    .map(SurfaceSessionId::raw)
            } else {
                state
                    .create_and_attach_surface_session()
                    .map(SurfaceSessionId::raw)
            };
            send_response(response, result);
        }
        DbusStateRequest::RenameSurfaceSession { id, name, response } => {
            let changed = state.rename_surface_session(SurfaceSessionId::from_raw(id), &name);
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
            state.register_extension(&role, metadata, sender_name),
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
        DbusStateRequest::ListPluginActors { response } => {
            send_response(
                response,
                Ok(dbus_plugin_actors(state.plugin_actor_summaries())),
            );
        }
        DbusStateRequest::ListPluginMailboxes { actor_id, response } => send_response(
            response,
            state
                .plugin_mailbox_summaries(&actor_id)
                .map(dbus_plugin_mailboxes),
        ),
        DbusStateRequest::SendPluginActorMessage {
            actor_id,
            message,
            sender_name,
            response,
        } => send_response(
            response,
            state
                .send_plugin_actor_message(&actor_id, sender_name, message)
                .map(|()| true),
        ),
        DbusStateRequest::SendPluginActorSlotMessage {
            actor_id,
            mailbox_slot,
            message,
            sender_name,
            response,
        } => send_response(
            response,
            state
                .send_plugin_actor_message_to_slot(&actor_id, &mailbox_slot, sender_name, message)
                .map(|()| true),
        ),
        DbusStateRequest::RegisterStatusPublisher {
            role,
            metadata,
            sender_name,
            response,
        } => send_response(
            response,
            state.register_status_publisher(&role, metadata, sender_name),
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
            state.publish_status(&publisher, &channel, &status_state, &label, metadata),
        ),
        DbusStateRequest::ClearStatus {
            publisher,
            channel,
            response,
        } => send_response(response, Ok(state.clear_status(&publisher, &channel))),
        DbusStateRequest::ListAgents { response } => {
            send_response(response, Ok(dbus_agents(state.agent_summaries())));
        }
        DbusStateRequest::RegisterAgent {
            kind,
            metadata,
            sender_name,
            response,
        } => send_response(
            response,
            state.register_agent(&kind, &metadata, sender_name),
        ),
        DbusStateRequest::UpdateAgent {
            agent_id,
            status,
            metadata,
            sender_name,
            response,
        } => send_response(
            response,
            state.update_agent(&agent_id, &status, metadata, sender_name),
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
            state.notify_agent(AgentNotification {
                agent_id,
                title,
                subtitle,
                body,
                level,
                metadata,
                sender: sender_name,
            }),
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
        } => send_response(
            response,
            state.unregister_agent_by_identity(&kind, &metadata),
        ),
        DbusStateRequest::RegisterWindow {
            extension_id,
            window_identity,
            role,
            sender_name,
            response,
        } => send_response(
            response,
            state.register_extension_window(extension_id, window_identity, &role, sender_name),
        ),
        DbusStateRequest::ReplaceRole {
            window_id,
            role,
            options,
            sender_name,
            response,
        } => send_response(
            response,
            state.replace_window_role(&window_id, &role, &options, sender_name),
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
            state.request_shortcut_grant(&extension_id, &shortcut, scope, label, sender_name),
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

    if let Err(error) = crate::frame_export::publish_catalog_snapshot(state) {
        warn!(
            ?error,
            "publish presentation catalog after DBus request failed"
        );
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

static FAST_PATH_GENERATION: AtomicU64 = AtomicU64::new(1);

fn open_presentation_fast_path(state: &mut Brood) -> Result<DbusFastPath> {
    let generation = FAST_PATH_GENERATION.fetch_add(1, Ordering::Relaxed);
    let active_session = if state.surface_session_count() == 0 {
        0
    } else {
        state.active_surface_session().raw()
    };
    let input_file = brood_ipc::CriticalLaneFile::create(generation)?;
    let frame_file = brood_ipc::CriticalLaneFile::create(generation)?;
    let catalog_file = brood_ipc::CriticalLaneFile::create(generation)?;
    let actor_message_file = brood_ipc::ActorMessageLaneFile::create(generation)?;
    let presentation_message_file = brood_ipc::ActorMessageLaneFile::create(generation)?;
    let input_notifications = brood_ipc::EventNotifier::create()?;
    let frame_notifications = brood_ipc::EventNotifier::create()?;
    let catalog_notifications = brood_ipc::EventNotifier::create()?;
    let actor_message_notifications = brood_ipc::EventNotifier::create()?;
    let presentation_message_notifications = brood_ipc::EventNotifier::create()?;
    let input_fd = input_file.as_fd().try_clone_to_owned()?;
    let input_notify_fd = input_notifications.as_fd().try_clone_to_owned()?;
    let frame_fd = frame_file.as_fd().try_clone_to_owned()?;
    let frame_notify_fd = frame_notifications.as_fd().try_clone_to_owned()?;
    let catalog_fd = catalog_file.as_fd().try_clone_to_owned()?;
    let catalog_notify_fd = catalog_notifications.as_fd().try_clone_to_owned()?;
    let actor_message_fd = actor_message_file.as_fd().try_clone_to_owned()?;
    let actor_message_notify_fd = actor_message_notifications.as_fd().try_clone_to_owned()?;
    let presentation_message_fd = presentation_message_file.as_fd().try_clone_to_owned()?;
    let presentation_message_notify_fd = presentation_message_notifications
        .as_fd()
        .try_clone_to_owned()?;
    let input = brood_ipc::CriticalConsumer::map(input_file.as_fd(), generation)?;
    let frames = brood_ipc::CriticalProducer::map(frame_file.as_fd(), generation)?;
    let catalog = brood_ipc::CriticalProducer::map(catalog_file.as_fd(), generation)?;
    let actor_messages =
        brood_ipc::ActorMessageConsumer::map(actor_message_file.as_fd(), generation)?;
    let presentation_messages =
        brood_ipc::ActorMessageProducer::map(presentation_message_file.as_fd(), generation)?;
    let fast_path = crate::state::PresentationFastPath {
        generation,
        input,
        frames,
        catalog,
        actor_messages,
        presentation_messages,
        frame_notifications,
        catalog_notifications,
        catalog_revision: 0,
        last_catalog_payload: Vec::new(),
        presentation_message_notifications,
        presentation_message_sequence: 0,
        deferred_presentation_messages: std::collections::VecDeque::new(),
        resources: std::collections::HashMap::new(),
        buffers: std::collections::HashMap::new(),
        resource_app_ids: std::collections::HashMap::new(),
        window_buffers: std::collections::HashMap::new(),
        next_resource_id: 1,
        last_presented_sequence: None,
        in_flight_sequence: None,
        pending_redraw: false,
    };
    state.dmabuf.fast_path = Some(fast_path);
    state.reconfigure_focus_stack();
    crate::frame_export::request_redraw(state);
    state
        .loop_handle
        .insert_source(
            Generic::new(input_notifications, Interest::READ, Mode::Level),
            |_, notifications, state| {
                if let Err(error) = notifications.drain() {
                    warn!(?error, "drain fast-path input notification failed");
                }
                drain_fast_path_input(state);
                Ok(PostAction::Continue)
            },
        )
        .map_err(|error| anyhow::anyhow!("install fast-path input notification: {error}"))?;
    state
        .loop_handle
        .insert_source(
            Generic::new(actor_message_notifications, Interest::READ, Mode::Level),
            |_, notifications, state| {
                if let Err(error) = notifications.drain() {
                    warn!(?error, "drain actor-message notification failed");
                }
                drain_fast_path_actor_messages(state);
                Ok(PostAction::Continue)
            },
        )
        .map_err(|error| anyhow::anyhow!("install actor-message notification: {error}"))?;
    Ok((
        generation,
        active_session,
        input_fd.into(),
        input_notify_fd.into(),
        frame_fd.into(),
        frame_notify_fd.into(),
        catalog_fd.into(),
        catalog_notify_fd.into(),
        actor_message_fd.into(),
        actor_message_notify_fd.into(),
        presentation_message_fd.into(),
        presentation_message_notify_fd.into(),
    ))
}

fn drain_fast_path_actor_messages(state: &mut Brood) {
    let Some(mut fast_path) = state.dmabuf.fast_path.take() else {
        return;
    };
    let mut messages = Vec::new();
    while let Some(message) = fast_path.actor_messages.try_recv() {
        if message.connection_generation() != fast_path.generation {
            continue;
        }
        let decoded = (|| {
            let sender = message.sender()?.to_string();
            let recipient = message.recipient()?.to_string();
            let body = message.body()?.to_vec();
            Ok::<_, brood_ipc::ActorMessageLaneError>((sender, recipient, body))
        })();
        let session = SurfaceSessionId::from_raw(message.session_id());
        drop(message);
        messages.push((session, decoded));
    }
    state.dmabuf.fast_path = Some(fast_path);
    for (session, decoded) in messages {
        match decoded {
            Ok((sender, recipient, body)) => {
                if let Err(error) =
                    state.send_session_actor_message(session, &recipient, &sender, body)
                {
                    warn!(
                        surface_session = session.raw(),
                        actor = %recipient,
                        error = %error,
                        "rejected presentation-to-session actor message"
                    );
                }
            }
            Err(error) => warn!(error = %error, "rejected malformed actor-message record"),
        }
    }
}

fn drain_fast_path_input(state: &mut Brood) {
    let Some(mut fast_path) = state.dmabuf.fast_path.take() else {
        return;
    };
    state.dmabuf.fast_path_input_draining = true;
    let mut request_redraw = false;
    let mut disconnected = false;
    while let Some(record) = fast_path.input.try_recv() {
        if record.connection_generation != fast_path.generation {
            continue;
        }
        let time = monotonic_time_msec(state);
        match record.opcode {
            brood_ipc::INPUT_KEYBOARD_KEY => state.forward_keyboard_key(
                u32::try_from(record.arguments[0])
                    .map_or(u32::MAX, |value| value)
                    .into(),
                if record.arguments[1] == 0 {
                    KeyState::Released
                } else {
                    KeyState::Pressed
                },
                time,
            ),
            brood_ipc::INPUT_KEYBOARD_RESET => state.reset_transient_input_state(),
            brood_ipc::INPUT_WINDOW_POINTER_MOTION => {
                state.forward_pointer_motion_to_pane(
                    record.arguments[0],
                    Point::from((
                        f64::from_bits(record.arguments[1]),
                        f64::from_bits(record.arguments[2]),
                    )),
                    time,
                );
            }
            brood_ipc::INPUT_WINDOW_POINTER_BUTTON => {
                let pane = record.arguments[0];
                state.forward_pointer_motion_to_pane(
                    pane,
                    Point::from((
                        f64::from_bits(record.arguments[3]),
                        f64::from_bits(record.arguments[4]),
                    )),
                    time,
                );
                state.forward_pointer_button_to_pane(
                    pane,
                    u32::try_from(record.arguments[1]).map_or(u32::MAX, |value| value),
                    if record.arguments[2] == 0 {
                        ButtonState::Released
                    } else {
                        ButtonState::Pressed
                    },
                    time,
                );
            }
            brood_ipc::INPUT_WINDOW_POINTER_AXIS => {
                let pane = record.arguments[0];
                state.forward_pointer_motion_to_pane(
                    pane,
                    Point::from((
                        f64::from_bits(record.arguments[3]),
                        f64::from_bits(record.arguments[4]),
                    )),
                    time,
                );
                let horizontal = f64::from_bits(record.arguments[1]);
                let vertical = f64::from_bits(record.arguments[2]);
                let frame = crate::input::managed_continuous_axis_frame(time, horizontal, vertical);
                state.forward_pointer_axis_to_pane(pane, frame);
            }
            brood_ipc::INPUT_WINDOW_POINTER_WHEEL => {
                let pane = record.arguments[0];
                state.forward_pointer_motion_to_pane(
                    pane,
                    Point::from((
                        f64::from_bits(record.arguments[3]),
                        f64::from_bits(record.arguments[4]),
                    )),
                    time,
                );
                let horizontal = brood_ipc::decode_i32_argument(record.arguments[1]);
                let vertical = brood_ipc::decode_i32_argument(record.arguments[2]);
                let frame = crate::input::managed_wheel_axis_frame(time, horizontal, vertical);
                state.forward_pointer_axis_to_pane(pane, frame);
            }
            brood_ipc::INPUT_WINDOW_POINTER_LEAVE => {
                state.forward_pointer_leave_managed_pane(time);
            }
            brood_ipc::INPUT_PRESENTATION_SIZE => {
                state.configure_external_presentation_output(
                    i32::try_from(record.arguments[0]).map_or(i32::MAX, |value| value),
                    i32::try_from(record.arguments[1]).map_or(i32::MAX, |value| value),
                    i32::try_from(record.arguments[2]).map_or(i32::MAX, |value| value),
                );
            }
            brood_ipc::INPUT_FRAME_PRESENTED => {
                let sequence = record.arguments[0];
                let accepted = fast_path.in_flight_sequence == Some(sequence);
                if accepted {
                    fast_path.last_presented_sequence = Some(sequence);
                    fast_path.in_flight_sequence = None;
                }
                let redraw_pending = accepted && fast_path.pending_redraw;
                if redraw_pending {
                    fast_path.pending_redraw = false;
                }
                drop(record);
                if accepted {
                    crate::frame_export::send_external_client_frame_callbacks(state);
                }
                if redraw_pending {
                    request_redraw = true;
                }
            }
            brood_ipc::INPUT_CLIENT_DISCONNECT => {
                disconnected = true;
                break;
            }
            brood_ipc::INPUT_PRESENTATION_FOCUS => {
                state.set_presentation_window_focused(record.arguments[0] != 0);
            }
            brood_ipc::INPUT_WINDOW_FOCUS
                if state.focus_pane_from_presentation_client(record.arguments[0]) =>
            {
                request_redraw = true;
            }
            brood_ipc::INPUT_WINDOW_RESIZE
                if state.configure_presentation_client_pane_size(
                    record.arguments[0],
                    i32::try_from(record.arguments[1]).map_or(i32::MAX, |value| value),
                    i32::try_from(record.arguments[2]).map_or(i32::MAX, |value| value),
                ) =>
            {
                request_redraw = true;
            }
            _ => {}
        }
    }
    if disconnected {
        state.dmabuf.fast_path_input_draining = false;
        state.set_presentation_window_focused(false);
        crate::frame_export::send_external_client_frame_callbacks(state);
        state.reconfigure_focus_stack();
    } else {
        state.dmabuf.fast_path = Some(fast_path);
        state.dmabuf.fast_path_input_draining = false;
    }
    if request_redraw || disconnected {
        crate::frame_export::request_redraw(state);
    }
    if let Err(error) = state.display_handle.flush_clients() {
        warn!(?error, "flush clients after fast-path input failed");
    }
}

fn latest_dmabuf_frame(state: &Brood) -> Result<DbusDmabufFrame> {
    let dmabuf = state
        .dmabuf
        .latest_client_buffer
        .as_ref()
        .context("no hosted DMA-BUF frame is available")?;
    dmabuf_frame_tuple(
        dmabuf,
        state.dmabuf.latest_client_buffer_sequence,
        String::new(),
    )
}

fn dmabuf_resource(state: &mut Brood, resource_id: u64) -> Result<DbusDmabufFrame> {
    let fast_path = state
        .dmabuf
        .fast_path
        .as_mut()
        .context("presentation fast path is unavailable")?;
    let dmabuf = fast_path
        .buffers
        .remove(&resource_id)
        .with_context(|| format!("unknown DMA-BUF resource {resource_id}"))?;
    let app_id = fast_path
        .resource_app_ids
        .remove(&resource_id)
        .unwrap_or_default();
    dmabuf_frame_tuple(&dmabuf, resource_id, app_id)
}

fn dmabuf_frame_tuple(
    dmabuf: &smithay::backend::allocator::dmabuf::Dmabuf,
    sequence: u64,
    app_id: String,
) -> Result<DbusDmabufFrame> {
    ensure!(
        dmabuf.num_planes() == 1,
        "happy-path presentation requires a single-plane DMA-BUF"
    );
    let size = dmabuf.size();
    let width = u32::try_from(size.w).context("DMA-BUF width is invalid")?;
    let height = u32::try_from(size.h).context("DMA-BUF height is invalid")?;
    let format = dmabuf.format();
    let modifier = if format.modifier == smithay::backend::allocator::Modifier::Invalid {
        smithay::backend::allocator::Modifier::Linear
    } else {
        format.modifier
    };
    let stride = dmabuf.strides().next().context("DMA-BUF has no stride")?;
    let offset = dmabuf.offsets().next().context("DMA-BUF has no offset")?;
    let handle = dmabuf
        .handles()
        .next()
        .context("DMA-BUF has no plane descriptor")?
        .try_clone_to_owned()
        .context("duplicate DMA-BUF plane descriptor")?;
    Ok((
        sequence,
        app_id,
        width,
        height,
        format.code as u32,
        brood_ipc::FrameColorSpace::Srgb.into(),
        u64::from(modifier),
        stride,
        offset,
        dmabuf.y_inverted(),
        handle.into(),
    ))
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

fn dbus_plugin_actors(
    actors: Vec<crate::plugin_runtime::PluginActorSummary>,
) -> Vec<DbusPluginActor> {
    actors
        .into_iter()
        .map(|actor| {
            (
                actor.id,
                actor.kind,
                actor.enqueued_messages,
                actor.dropped_messages,
                u64::try_from(actor.mailbox_capacity).unwrap_or(u64::MAX),
                actor.published_frames,
                actor.latest_frame,
                format_plugin_mailbox_slots(&actor.mailbox_slots),
            )
        })
        .collect()
}

fn dbus_plugin_mailboxes(
    mailboxes: Vec<crate::plugin_runtime::PluginMailboxSummary>,
) -> Vec<DbusPluginMailbox> {
    mailboxes
        .into_iter()
        .map(|mailbox| {
            (
                mailbox.slot,
                mailbox.enqueued_messages,
                mailbox.dropped_messages,
                u64::try_from(mailbox.capacity).unwrap_or(u64::MAX),
            )
        })
        .collect()
}

fn format_plugin_mailbox_slots(
    mailboxes: &[crate::plugin_runtime::PluginMailboxSummary],
) -> String {
    if mailboxes.is_empty() {
        return "none".to_string();
    }
    mailboxes
        .iter()
        .map(|mailbox| {
            format!(
                "{}:{}/{}/{}",
                mailbox.slot, mailbox.enqueued_messages, mailbox.dropped_messages, mailbox.capacity
            )
        })
        .collect::<Vec<_>>()
        .join(",")
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

pub fn status_json_string(state: &mut Brood) -> Result<String> {
    let snapshot = StatusSnapshot::from_state(state);
    serde_json::to_string(&snapshot.to_json()).context("serialize status snapshot")
}

fn execute_tokens(tokens: &[String], state: &mut Brood) -> Result<String> {
    let Some((command, args)) = tokens.split_first() else {
        bail!("empty control command");
    };

    let response = match command.as_str() {
        "handshake" => {
            let [client_version] = args else {
                bail!("usage: handshake <protocol-version>");
            };
            let client_version = client_version
                .parse::<u32>()
                .context("parse client presentation protocol version")?;
            ensure!(
                client_version == brood_cli::control::PRESENTATION_PROTOCOL_VERSION,
                "unsupported presentation protocol version {client_version}"
            );
            let handshake = brood_cli::control::Handshake {
                protocol_version: brood_cli::control::PRESENTATION_PROTOCOL_VERSION,
                server_pid: std::process::id(),
            };
            format!("{}\n", serde_json::to_string(&handshake)?)
        }
        "focus-presentation" if args.is_empty() => {
            let focused = state.focus_presentation_window();
            format!("OK focus-presentation focused={focused}\n")
        }
        "close-frontend" | "frontend-close" if args.is_empty() => {
            let requested = state.close_presentation_client();
            format!("OK frontend close requested={requested}\n")
        }
        "status" if args.is_empty() => {
            let snapshot = StatusSnapshot::from_state(state);
            format!(
                "OK wayland_display={} presentation={} windows={} total_windows={} active_surface_session={} surface_sessions={} focused_windows={} hosted_children={} frames={} commits={} input_events={} dmabuf={} dmabuf_rejected={} dmabuf_last_error={}\n",
                snapshot.wayland_display,
                snapshot.presentation,
                snapshot.windows,
                snapshot.total_windows,
                snapshot.active_surface_session,
                snapshot.surface_sessions,
                snapshot.focused_windows,
                snapshot.hosted_children,
                snapshot.frames,
                snapshot.commits,
                snapshot.input_events,
                snapshot.dmabuf,
                snapshot.dmabuf_rejected,
                snapshot.dmabuf_last_error,
            )
        }
        "status-json" if args.is_empty() => {
            format!("{}\n", status_json_string(state)?)
        }
        "quit" if args.is_empty() => {
            let closed_browsers = state.browsers.close_all().len();
            // Stop dispatching first and return the control reply promptly.
            // The compositor's post-loop teardown owns joining every hosted
            // actor, worker, completion task, and legacy plugin task.
            state.loop_signal.stop();
            format!("OK quit requested browsers_closed={closed_browsers}") + "\n"
        }
        "spawn" => {
            if args.is_empty() {
                bail!("spawn requires a command");
            }
            let token = state.spawn_hosted_app_argv(args)?;
            format!("OK spawned {} pane_id={}\n", args.join(" "), token.raw())
        }
        "spawn-in-session" => {
            let Some((session, argv)) = args.split_first() else {
                bail!("spawn-in-session requires a session id and command");
            };
            if argv.is_empty() {
                bail!("spawn-in-session requires a command");
            }
            let session = parse_u64(session, "session id")?;
            let token = state
                .spawn_hosted_app_argv_in_session(argv, SurfaceSessionId::from_raw(session))?;
            format!("OK spawned {} pane_id={}\n", argv.join(" "), token.raw())
        }
        "hosted-child-status" => {
            let [pane_id] = args else {
                bail!("hosted-child-status requires exactly one pane id");
            };
            let token = HostedChildToken::from_raw(parse_u64(pane_id, "pane id")?);
            let visible = state.hosted_child_visible(token);
            let status = match state.hosted_child_status(token) {
                HostedChildStatus::Launching => {
                    serde_json::json!({"pane_id": token.raw(), "state": "launching", "visible": visible})
                }
                HostedChildStatus::Running => {
                    serde_json::json!({"pane_id": token.raw(), "state": "running", "visible": visible})
                }
                HostedChildStatus::Exited { success, detail } => serde_json::json!({
                    "pane_id": token.raw(),
                    "state": "exited",
                    "visible": visible,
                    "success": success,
                    "detail": detail,
                }),
                HostedChildStatus::Unknown => {
                    serde_json::json!({"pane_id": token.raw(), "state": "unknown", "visible": visible})
                }
            };
            format!("{status}\n")
        }
        "plug" => {
            let command = PlugCommand::parse(args)?;
            let loaded_kind = plug_wasm_module(
                state,
                &command.path,
                command.kind,
                command.instance_id.as_deref(),
            )?;
            format_plug_success(loaded_kind, command.instance_id.as_deref(), &command.path)
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
        "client" | "pane" => match args {
            [subcommand] if subcommand == "close" => {
                let changed = state.close_active_window();
                format!("OK client close target=active changed={changed}\n")
            }
            [subcommand, target] if subcommand == "close" && target == "active" => {
                let changed = state.close_active_window();
                format!("OK client close target=active changed={changed}\n")
            }
            [subcommand, pane] if subcommand == "close" => {
                let pane = parse_u64(pane, "pane id")?;
                let changed = state.close_pane(pane);
                format!("OK client close pane={pane} changed={changed}\n")
            }
            _ => bail!("usage: client close [active|<pane-id>]"),
        },
        "close-client" | "close-pane" => match args {
            [] => {
                let changed = state.close_active_window();
                format!("OK client close target=active changed={changed}\n")
            }
            [target] if target == "active" => {
                let changed = state.close_active_window();
                format!("OK client close target=active changed={changed}\n")
            }
            [pane] => {
                let pane = parse_u64(pane, "pane id")?;
                let changed = state.close_pane(pane);
                format!("OK client close pane={pane} changed={changed}\n")
            }
            _ => bail!("usage: close-client [active|<pane-id>]"),
        },
        "close-session" => close_surface_session_command(args, state)?,
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
                )?;
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
            [subcommand, target @ ..] if subcommand == "close" => {
                close_surface_session_command(target, state)?
            }
            [subcommand, name @ ..] if subcommand == "new" => {
                let name = joined_name(name);
                if let Some(name) = name {
                    state.create_and_attach_named_surface_session(Some(name))?;
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
                "usage: surface-session list-json|dir-list-json <dir>|dir-get <dir>|dir-set <dir> <id> [default|keep-default]|next|last-focused|close [active|<query>]|new|switch <query>"
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
            let (destination, address) = state
                .nested_test_target()
                .context("nested inner DBus destination not configured")?;
            if args.is_empty() {
                bail!("usage: nested-inner <command...>");
            }
            let target = ControlTarget::new(destination.to_string(), address.map(str::to_string));
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
            _ => bail!("usage: debug dump <dir> [state,surfaces]"),
        },
        _ => bail!("unknown command: {}", tokens.join(" ")),
    };

    Ok(response)
}

fn close_surface_session_command(args: &[String], state: &mut Brood) -> Result<String> {
    let session = match args {
        [] => state.active_surface_session(),
        [target] if target == "active" => state.active_surface_session(),
        [query] => state
            .resolve_surface_session(query)
            .with_context(|| format!("unknown surface session {query:?}"))?,
        _ => bail!("usage: close-session [active|<query>]"),
    };
    let closed = session.raw();
    let changed = state.close_surface_session(session);
    Ok(format!(
        "OK surface-session closed={closed} active={} changed={changed}\n",
        state.active_surface_session().raw()
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WasmPlugKind {
    Auto,
    Plugin,
}

impl WasmPlugKind {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "plugin" | "wasm-plugin" => Ok(Self::Plugin),
            _ => bail!("plug kind must be auto or plugin"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PlugCommand {
    kind: WasmPlugKind,
    instance_id: Option<String>,
    path: PathBuf,
}

impl PlugCommand {
    fn parse(args: &[String]) -> Result<Self> {
        let mut kind = WasmPlugKind::Auto;
        let mut instance_id = None;
        let mut path = None;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--kind" | "-k" => {
                    let Some(value) = args.next() else {
                        bail!("plug --kind requires a value");
                    };
                    kind = WasmPlugKind::parse(value)?;
                }
                arg if arg.starts_with("--kind=") => {
                    let value = arg.trim_start_matches("--kind=");
                    kind = WasmPlugKind::parse(value)?;
                }
                "--id" => {
                    let Some(value) = args.next() else {
                        bail!("plug --id requires a value");
                    };
                    ensure!(!value.is_empty(), "plug --id must not be empty");
                    ensure!(
                        instance_id.is_none(),
                        "plug --id may only be specified once"
                    );
                    instance_id = Some(value.clone());
                }
                arg if arg.starts_with("--id=") => {
                    let value = arg.trim_start_matches("--id=");
                    ensure!(!value.is_empty(), "plug --id must not be empty");
                    ensure!(
                        instance_id.is_none(),
                        "plug --id may only be specified once"
                    );
                    instance_id = Some(value.to_string());
                }
                arg if arg.starts_with('-') => {
                    bail!("unknown plug option {arg}");
                }
                arg => {
                    ensure!(path.is_none(), "plug accepts exactly one WASM module path");
                    path = Some(PathBuf::from(arg));
                }
            }
        }

        let path = path.context("usage: plug [--kind auto|plugin] [--id ID] <path>")?;
        Ok(Self {
            kind,
            instance_id,
            path,
        })
    }
}

fn format_plug_success(kind: &str, instance_id: Option<&str>, path: &Path) -> String {
    instance_id.map_or_else(
        || format!("OK plug kind={} path={}\n", kind, path.display()),
        |instance_id| {
            format!(
                "OK plug kind={} id={} path={}\n",
                kind,
                instance_id,
                path.display()
            )
        },
    )
}

fn plug_wasm_module(
    state: &mut Brood,
    path: &Path,
    kind: WasmPlugKind,
    instance_id: Option<&str>,
) -> Result<&'static str> {
    ensure!(
        path.exists(),
        "WASM module does not exist: {}",
        path.display()
    );
    match kind {
        WasmPlugKind::Plugin | WasmPlugKind::Auto => {
            if let Some(instance_id) = instance_id {
                state.replace_wasm_plugin_as(instance_id, path)?;
            } else {
                state.replace_wasm_plugin(path)?;
            }
            Ok("plugin")
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
    presentation_window_focused: bool,
    windows: usize,
    total_windows: usize,
    active_surface_session: u64,
    active_surface_session_name: String,
    surface_sessions: usize,
    focused_windows: usize,
    focused_window_index: Option<usize>,
    focused_surface_id: String,
    requested_fullscreen: usize,
    focused_fullscreen: bool,
    browsers: usize,
    agents: Vec<crate::state::AgentSummary>,
    plugin_actors: Vec<DbusPluginActor>,
    plugin_actor_messages: u64,
    plugin_actor_failures: u64,
    plugin_actor_last_status: String,
    preview1_compat_active: bool,
    preview1_compat_generation: u64,
    preview1_compat_mailbox_queued: u64,
    preview1_compat_mailbox_dropped: u64,
    preview1_compat_mailbox_capacity: usize,
    preview1_compat_mailbox_dispatched: u64,
    preview1_compat_completion_delivered: u64,
    preview1_compat_notifications_delivered: u64,
    preview1_compat_notifications: usize,
    preview1_compat_pollables: usize,
    preview1_compat_wait_registrations: usize,
    preview1_compat_pending_notification_events: usize,
    preview1_compat_filesystem_watches: usize,
    preview1_compat_max_filesystem_watches: usize,
    preview1_compat_max_filesystem_watch_events: usize,
    preview1_compat_filesystem_watch_overflows: u64,
    preview1_compat_filesystem_watch_rescans: u64,
    preview1_compat_workers_started: u64,
    preview1_compat_workers_active: usize,
    preview1_compat_worker_reads_started: u64,
    preview1_compat_workers_finished: u64,
    preview1_compat_workers_failed: u64,
    preview1_compat_workers_cancelled: u64,
    preview1_compat_last_worker_value: u64,
    filesystem_watcher_registrations: usize,
    filesystem_watcher_subscriptions: usize,
    filesystem_watcher_setup_tasks: usize,
    filesystem_watcher_overflows: u64,
    filesystem_watcher_rescans: u64,
    hosted_filesystem_watches: usize,
    hosted_filesystem_watch_capacity: usize,
    hosted_filesystem_watch_event_capacity: usize,
    hosted_filesystem_watch_overflows: u64,
    hosted_filesystem_watch_rescans: u64,
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
    max_commit_to_frame_us: u128,
    last_commit_to_submit_us: Option<u128>,
    max_commit_to_submit_us: u128,
    client_frame_callback_commit_count: u64,
    last_client_frame_callback_batch_callbacks: u64,
    max_client_frame_callback_batch_callbacks: u64,
    last_client_frame_callback_to_commit_us: Option<u128>,
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
    max_input_to_frame_us: u128,
    last_input_to_submit_us: Option<u128>,
    max_input_to_submit_us: u128,
    input_commit_frame_count: u64,
    input_without_commit_frame_count: u64,
    last_input_to_commit_us: Option<u128>,
    max_input_to_commit_us: u128,
    last_input_commit_to_submit_us: Option<u128>,
    max_input_commit_to_submit_us: u128,
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
    fn from_state(state: &mut Brood) -> Self {
        state.refresh_window_state();

        let last_input_to_frame_us = state.stats.last_input_to_frame_micros;
        let last_commit_to_frame_us = state.stats.last_commit_to_frame_micros;
        let last_input_to_submit_us = state.stats.last_input_to_submit_micros;
        let last_commit_to_submit_us = state.stats.last_commit_to_submit_micros;
        let last_input_to_commit_us = state.stats.last_input_to_commit_micros;
        let last_input_commit_to_submit_us = state.stats.last_input_commit_to_submit_micros;
        let last_client_frame_callback_to_commit_us =
            state.stats.last_client_frame_callback_to_commit_micros;
        state.refresh_hosted_children();
        let presentation = if state.dmabuf.fast_path.is_some() {
            "Attached"
        } else {
            "Detached"
        };
        let dmabuf = state
            .dmabuf
            .protocol_version
            .map_or_else(|| "none".to_string(), |version| format!("v{version}"));
        let explicit_sync = state
            .dmabuf
            .explicit_sync_protocol_version
            .map_or_else(|| "none".to_string(), |version| format!("v{version}"));
        let preview1_compat_stats = state.preview1_compat_stats();
        let filesystem_watcher_stats = state.filesystem_watcher_stats();
        let hosted_filesystem_watch_stats = state.hosted_filesystem_watch_stats();

        Self {
            wayland_display: state.socket_name.to_string_lossy().into_owned(),
            wayland_client_insert_errors: state.stats.wayland_client_insert_errors,
            wayland_dispatch_errors: state.stats.wayland_dispatch_errors,
            xdg_configure_errors: state.stats.xdg_configure_errors,
            presentation,
            presentation_window_focused: state.presentation_window_focused(),
            windows: state.active_surface_session_window_count(),
            total_windows: state.space.elements().count(),
            active_surface_session: state.active_surface_session().raw(),
            active_surface_session_name: sanitize_ctl_field(state.active_surface_session_name()),
            surface_sessions: state.surface_session_count(),
            focused_windows: state.focused_window_count(),
            focused_window_index: state.focused_window_index(),
            focused_surface_id: state
                .focused_surface_id()
                .unwrap_or_else(|| "none".to_string()),
            requested_fullscreen: state.requested_fullscreen_count(),
            focused_fullscreen: state.focused_window_fullscreen(),
            browsers: state.browsers.len(),
            agents: state.agent_summaries(),
            plugin_actors: dbus_plugin_actors(state.plugin_actor_summaries()),
            plugin_actor_messages: state.plugin_actor_message_count(),
            plugin_actor_failures: state.plugin_actor_failure_count(),
            plugin_actor_last_status: sanitize_ctl_field(
                state.plugin_actor_last_status().unwrap_or("none"),
            ),
            preview1_compat_active: preview1_compat_stats.is_some(),
            preview1_compat_generation: preview1_compat_stats.map_or(0, |stats| stats.generation),
            preview1_compat_mailbox_queued: preview1_compat_stats
                .map_or(0, |stats| stats.mailbox_queued),
            preview1_compat_mailbox_dropped: preview1_compat_stats
                .map_or(0, |stats| stats.mailbox_dropped),
            preview1_compat_mailbox_capacity: preview1_compat_stats
                .map_or(0, |stats| stats.mailbox_capacity),
            preview1_compat_mailbox_dispatched: preview1_compat_stats
                .map_or(0, |stats| stats.mailbox_dispatched),
            preview1_compat_completion_delivered: preview1_compat_stats
                .map_or(0, |stats| stats.completion_delivered),
            preview1_compat_notifications_delivered: preview1_compat_stats
                .map_or(0, |stats| stats.notifications_delivered),
            preview1_compat_notifications: preview1_compat_stats
                .map_or(0, |stats| stats.notifications),
            preview1_compat_pollables: preview1_compat_stats.map_or(0, |stats| stats.pollables),
            preview1_compat_wait_registrations: preview1_compat_stats
                .map_or(0, |stats| stats.wait_registrations),
            preview1_compat_pending_notification_events: preview1_compat_stats
                .map_or(0, |stats| stats.pending_notification_events),
            preview1_compat_filesystem_watches: preview1_compat_stats
                .map_or(0, |stats| stats.filesystem_watches),
            preview1_compat_max_filesystem_watches: preview1_compat_stats
                .map_or(0, |stats| stats.max_filesystem_watches),
            preview1_compat_max_filesystem_watch_events: preview1_compat_stats
                .map_or(0, |stats| stats.max_filesystem_watch_events),
            preview1_compat_filesystem_watch_overflows: preview1_compat_stats
                .map_or(0, |stats| stats.filesystem_watch_overflows),
            preview1_compat_filesystem_watch_rescans: preview1_compat_stats
                .map_or(0, |stats| stats.filesystem_watch_rescans),
            preview1_compat_workers_started: preview1_compat_stats
                .map_or(0, |stats| stats.workers_started),
            preview1_compat_workers_active: preview1_compat_stats
                .map_or(0, |stats| stats.workers_active),
            preview1_compat_worker_reads_started: preview1_compat_stats
                .map_or(0, |stats| stats.worker_reads_started),
            preview1_compat_workers_finished: preview1_compat_stats
                .map_or(0, |stats| stats.workers_finished),
            preview1_compat_workers_failed: preview1_compat_stats
                .map_or(0, |stats| stats.workers_failed),
            preview1_compat_workers_cancelled: preview1_compat_stats
                .map_or(0, |stats| stats.workers_cancelled),
            preview1_compat_last_worker_value: preview1_compat_stats
                .map_or(0, |stats| stats.last_worker_value),
            filesystem_watcher_registrations: filesystem_watcher_stats.registrations,
            filesystem_watcher_subscriptions: filesystem_watcher_stats.subscriptions,
            filesystem_watcher_setup_tasks: filesystem_watcher_stats.setup_tasks,
            filesystem_watcher_overflows: filesystem_watcher_stats.overflows,
            filesystem_watcher_rescans: filesystem_watcher_stats.rescans,
            hosted_filesystem_watches: hosted_filesystem_watch_stats.watches,
            hosted_filesystem_watch_capacity: hosted_filesystem_watch_stats.max_watches,
            hosted_filesystem_watch_event_capacity: hosted_filesystem_watch_stats.max_events,
            hosted_filesystem_watch_overflows: hosted_filesystem_watch_stats.overflows,
            hosted_filesystem_watch_rescans: hosted_filesystem_watch_stats.rescans,
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
            max_commit_to_frame_us: state.stats.max_commit_to_frame_micros,
            last_commit_to_submit_us,
            max_commit_to_submit_us: state.stats.max_commit_to_submit_micros,
            client_frame_callback_commit_count: state.stats.client_frame_callback_commit_count,
            last_client_frame_callback_batch_callbacks: state
                .stats
                .last_client_frame_callback_batch_callbacks,
            max_client_frame_callback_batch_callbacks: state
                .stats
                .max_client_frame_callback_batch_callbacks,
            last_client_frame_callback_to_commit_us,
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
            max_input_to_frame_us: state.stats.max_input_to_frame_micros,
            last_input_to_submit_us,
            max_input_to_submit_us: state.stats.max_input_to_submit_micros,
            input_commit_frame_count: state.stats.input_commit_frame_count,
            input_without_commit_frame_count: state.stats.input_without_commit_frame_count,
            last_input_to_commit_us,
            max_input_to_commit_us: state.stats.max_input_to_commit_micros,
            last_input_commit_to_submit_us,
            max_input_commit_to_submit_us: state.stats.max_input_commit_to_submit_micros,
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

fn monotonic_time_msec(state: &Brood) -> u32 {
    let millis = state.start_time.elapsed().as_millis();
    u32::try_from(millis.min(u128::from(u32::MAX))).expect("millis clamped to u32 range")
}

fn parse_u64(value: &str, name: &str) -> Result<u64> {
    value
        .parse()
        .with_context(|| format!("{name} must be an unsigned integer"))
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
    fn from_state(state: &mut Brood) -> Self {
        state.refresh_window_state();
        Self {
            presentation: if state.dmabuf.fast_path.is_some() {
                "Attached".to_string()
            } else {
                "Detached".to_string()
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
    fn from_state(state: &mut Brood, directory: &Path) -> Self {
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
        .arg("brood session> ")
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
