use std::{
    cell::RefCell,
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    os::fd::{AsFd, OwnedFd},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use zbus::proxy;

use crate::{
    BROOD_SESSION_ENV, BROODD_DBUS_ADDRESS_ENV, BROODD_DBUS_DESTINATION_ENV,
    DEFAULT_BROOD_DBUS_NAME,
};

pub const CONTROL_OBJECT_PATH: &str = "/org/brood/Session";
pub const CONTROL_INTERFACE: &str = "org.brood.Session.Control";
pub use brood_protocol::PRESENTATION_PROTOCOL_VERSION;
const SERVER_START_ATTEMPTS: usize = 180;
const SERVER_START_INTERVAL: Duration = Duration::from_millis(100);

type LocalTransport = brood_ipc::PacketTransport<
    { brood_ipc::LOCAL_CONTROL_MAX_BYTES },
    { brood_ipc::LOCAL_CONTROL_MAX_FDS },
>;

thread_local! {
    static LOCAL_CONNECTIONS: RefCell<BTreeMap<PathBuf, LocalTransport>> =
        const { RefCell::new(BTreeMap::new()) };
}

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
    zbus::zvariant::OwnedFd,
);
type DbusSurfaceSession = (u64, String, bool, u64);
type DbusPresentationWindow = (u64, u64, String);

#[proxy(
    interface = "org.brood.Session.Control",
    default_path = "/org/brood/Session"
)]
trait BroodControl {
    fn command(&self, argv: Vec<String>, json: bool) -> zbus::Result<String>;
    fn latest_dmabuf_frame(&self) -> zbus::Result<DbusDmabufFrame>;
}

#[proxy(interface = "org.brood.Session", default_path = "/org/brood/Session")]
trait BroodSession {
    fn list_surface_sessions(&self) -> zbus::Result<Vec<DbusSurfaceSession>>;
    fn list_presentation_windows(&self) -> zbus::Result<Vec<DbusPresentationWindow>>;
    fn activate_surface_session(&self, id: u64) -> zbus::Result<bool>;
    fn create_surface_session(&self, name: String) -> zbus::Result<u64>;
    fn create_managed_worktree_session(
        &self,
        source_session: u64,
        branch: String,
    ) -> zbus::Result<u64>;
    fn rename_surface_session(&self, id: u64, name: String) -> zbus::Result<bool>;
    fn close_surface_session(&self, id: u64) -> zbus::Result<bool>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceSessionSummary {
    pub id: u64,
    pub name: String,
    pub active: bool,
    pub windows: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentationWindowSummary {
    pub pane: u64,
    pub session: u64,
    pub app_id: String,
    pub size_authority: brood_protocol::ManagedWindowSizeAuthority,
    pub automation: brood_protocol::ManagedWindowAutomationState,
}

#[derive(Clone, Debug)]
pub struct ControlTarget {
    pub destination: String,
    pub address: Option<String>,
}

impl ControlTarget {
    #[must_use]
    pub const fn new(destination: String, address: Option<String>) -> Self {
        Self {
            destination,
            address,
        }
    }

    /// Private socket used only by Brood-owned local clients.
    #[must_use]
    pub fn local_ipc_path(&self) -> PathBuf {
        let effective_address = self
            .address
            .as_deref()
            .map(str::to_string)
            .or_else(|| env::var(BROODD_DBUS_ADDRESS_ENV).ok())
            .or_else(|| env::var("DBUS_SESSION_BUS_ADDRESS").ok())
            .unwrap_or_default();
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in self
            .destination
            .bytes()
            .chain([0])
            .chain(effective_address.as_str().bytes())
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        let bus_runtime = effective_address.split(',').find_map(|field| {
            let path = field.strip_prefix("unix:path=")?;
            Path::new(path).parent().map(Path::to_path_buf)
        });
        bus_runtime
            .or_else(|| env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
            .unwrap_or_else(env::temp_dir)
            .join("brood")
            .join(format!("presentation-{hash:016x}.sock"))
    }

    #[must_use]
    pub fn session_marker(&self) -> String {
        self.session_marker_with_session(None)
    }

    #[must_use]
    pub fn session_marker_with_session(&self, surface_session_id: Option<u64>) -> String {
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Handshake {
    pub protocol_version: u32,
    pub server_pid: u32,
}

#[derive(Debug)]
pub struct PresentationFastPath {
    pub generation: u64,
    pub active_session: u64,
    pub profile: brood_protocol::TransportProfile,
    pub input_fd: OwnedFd,
    pub input_notify_fd: OwnedFd,
    pub frame_fd: OwnedFd,
    pub frame_notify_fd: OwnedFd,
    pub catalog_fd: OwnedFd,
    pub catalog_notify_fd: OwnedFd,
    pub actor_message_fd: OwnedFd,
    pub actor_message_notify_fd: OwnedFd,
    pub presentation_message_fd: OwnedFd,
    pub presentation_message_notify_fd: OwnedFd,
    pub backend_request_fd: OwnedFd,
    pub backend_request_notify_fd: OwnedFd,
    pub backend_response_fd: OwnedFd,
    pub backend_response_notify_fd: OwnedFd,
}

#[derive(Debug)]
pub struct SharedMemoryFrame {
    pub app_id: String,
    pub image: brood_ipc::SharedImageConsumer,
}

#[derive(Debug)]
pub struct DmabufFrame {
    pub sequence: u64,
    pub app_id: String,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub color_space: brood_ipc::FrameColorSpace,
    pub modifier: u64,
    pub stride: u32,
    pub offset: u32,
    pub y_inverted: bool,
    pub fd: OwnedFd,
}

#[derive(Clone, Debug)]
pub struct ServerLaunch {
    pub executable: PathBuf,
    pub current_directory: PathBuf,
    pub config: Option<PathBuf>,
}

impl ServerLaunch {
    pub fn discover() -> Result<Self> {
        let current_executable = env::current_exe().context("resolve current executable")?;
        let executable = env::var_os("BROODD_BIN").map_or_else(
            || current_executable.with_file_name("brood-sessiond"),
            PathBuf::from,
        );
        let current_directory = env::current_dir().context("determine current directory")?;
        Ok(Self {
            executable,
            current_directory,
            config: None,
        })
    }
}

#[must_use]
pub fn resolve_control_target(
    destination: Option<String>,
    address: Option<String>,
) -> ControlTarget {
    let marker = env::var(BROOD_SESSION_ENV).ok();
    let destination = destination
        .or_else(|| env::var(BROODD_DBUS_DESTINATION_ENV).ok())
        .or_else(|| {
            marker
                .as_deref()
                .and_then(|value| session_marker_value(value, "dbus-destination"))
        })
        .unwrap_or_else(|| DEFAULT_BROOD_DBUS_NAME.to_string());
    let address = address
        .or_else(|| env::var(BROODD_DBUS_ADDRESS_ENV).ok())
        .or_else(|| {
            marker
                .as_deref()
                .and_then(|value| session_marker_value(value, "dbus-address"))
        });
    ControlTarget::new(destination, address)
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

    let connection = connection(target)?;
    let proxy = BroodControlProxyBlocking::builder(&connection)
        .destination(target.destination.as_str())?
        .path(CONTROL_OBJECT_PATH)?
        .interface(CONTROL_INTERFACE)?
        .build()
        .context("build Brood DBus control proxy")?;
    proxy
        .command(command.to_vec(), json)
        .with_context(|| format!("call Brood DBus control command on {}", target.destination))
}

/// Execute a Brood-internal control operation without entering the public D-Bus API.
pub fn request_internal_command(
    target: &ControlTarget,
    command: &[String],
    json: bool,
) -> Result<String> {
    if json {
        let encoded = command.join(" ");
        let _: Vec<String> =
            serde_json::from_str(&encoded).context("validate JSON control argv")?;
    }
    match local_request(
        target,
        &brood_ipc::LocalControlRequest::Control {
            argv: command.to_vec(),
            json,
        },
    )?
    .message
    {
        brood_ipc::LocalControlValue::Control(output) => Ok(output),
        _ => bail!("private IPC returned the wrong control response"),
    }
}

pub fn install_session_init_artifact(target: &ControlTarget, path: &Path) -> Result<bool> {
    match local_request(
        target,
        &brood_ipc::LocalControlRequest::InstallSessionInitArtifact {
            path: path.display().to_string(),
        },
    )?
    .message
    {
        brood_ipc::LocalControlValue::Bool(installed) => Ok(installed),
        _ => bail!("private IPC returned the wrong artifact-install response"),
    }
}

pub fn take_realm_artifact_requests(target: &ControlTarget) -> Result<Vec<(u64, String)>> {
    match local_request(
        target,
        &brood_ipc::LocalControlRequest::TakeRealmArtifactRequests,
    )?
    .message
    {
        brood_ipc::LocalControlValue::RealmArtifactRequests(requests) => Ok(requests),
        _ => bail!("private IPC returned the wrong artifact-request response"),
    }
}

pub fn complete_realm_artifact_request(
    target: &ControlTarget,
    request: u64,
    success: bool,
    message: String,
) -> Result<bool> {
    match local_request(
        target,
        &brood_ipc::LocalControlRequest::CompleteRealmArtifactRequest {
            request,
            success,
            message,
        },
    )?
    .message
    {
        brood_ipc::LocalControlValue::Bool(completed) => Ok(completed),
        _ => bail!("private IPC returned the wrong artifact-completion response"),
    }
}

fn session_proxy<'a>(
    connection: &'a zbus::blocking::Connection,
    target: &'a ControlTarget,
) -> Result<BroodSessionProxyBlocking<'a>> {
    BroodSessionProxyBlocking::builder(connection)
        .destination(target.destination.as_str())?
        .path(CONTROL_OBJECT_PATH)?
        .build()
        .context("build Brood session proxy")
}

pub fn list_surface_sessions(target: &ControlTarget) -> Result<Vec<SurfaceSessionSummary>> {
    let connection = connection(target)?;
    session_proxy(&connection, target)?
        .list_surface_sessions()
        .context("list Brood surface sessions")
        .map(|sessions| {
            sessions
                .into_iter()
                .map(|(id, name, active, windows)| SurfaceSessionSummary {
                    id,
                    name,
                    active,
                    windows,
                })
                .collect()
        })
}

pub fn list_presentation_windows(target: &ControlTarget) -> Result<Vec<PresentationWindowSummary>> {
    let connection = connection(target)?;
    session_proxy(&connection, target)?
        .list_presentation_windows()
        .context("list Brood presentation windows")
        .map(|windows| {
            windows
                .into_iter()
                .map(|(pane, session, app_id)| PresentationWindowSummary {
                    pane,
                    session,
                    app_id,
                    size_authority: brood_protocol::ManagedWindowSizeAuthority::Presentation,
                    automation: brood_protocol::ManagedWindowAutomationState::Normal,
                })
                .collect()
        })
}

pub fn activate_surface_session(target: &ControlTarget, id: u64) -> Result<bool> {
    let connection = connection(target)?;
    session_proxy(&connection, target)?
        .activate_surface_session(id)
        .with_context(|| format!("activate Brood surface session {id}"))
}

pub fn create_surface_session(target: &ControlTarget, name: String) -> Result<u64> {
    let connection = connection(target)?;
    session_proxy(&connection, target)?
        .create_surface_session(name)
        .context("create Brood surface session")
}

pub fn create_managed_worktree_session(
    target: &ControlTarget,
    source_session: u64,
    branch: String,
) -> Result<u64> {
    let connection = connection(target)?;
    session_proxy(&connection, target)?
        .create_managed_worktree_session(source_session, branch)
        .context("create managed worktree session")
}

pub fn rename_surface_session(target: &ControlTarget, id: u64, name: String) -> Result<bool> {
    let connection = connection(target)?;
    session_proxy(&connection, target)?
        .rename_surface_session(id, name)
        .with_context(|| format!("rename Brood surface session {id}"))
}

pub fn close_surface_session(target: &ControlTarget, id: u64) -> Result<bool> {
    let connection = connection(target)?;
    session_proxy(&connection, target)?
        .close_surface_session(id)
        .with_context(|| format!("close Brood surface session {id}"))
}

pub fn open_presentation_fast_path(
    target: &ControlTarget,
    frame_transports: &[brood_protocol::FrameTransportKind],
) -> Result<PresentationFastPath> {
    let response = local_request(
        target,
        &brood_ipc::LocalControlRequest::OpenPresentation {
            frame_transports: frame_transports.to_vec(),
        },
    )?;
    let brood_ipc::LocalControlValue::Presentation {
        generation,
        active_session,
        frame_transport: frames,
    } = response.message
    else {
        bail!("private IPC returned the wrong presentation response");
    };
    let mut descriptors = response.descriptors.into_iter();
    let mut next = || {
        descriptors
            .next()
            .context("presentation response omitted a descriptor")
    };
    Ok(PresentationFastPath {
        generation,
        active_session,
        profile: brood_protocol::TransportProfile {
            carrier: brood_protocol::CarrierKind::LocalSharedMemory,
            frames,
        },
        input_fd: next()?,
        input_notify_fd: next()?,
        frame_fd: next()?,
        frame_notify_fd: next()?,
        catalog_fd: next()?,
        catalog_notify_fd: next()?,
        actor_message_fd: next()?,
        actor_message_notify_fd: next()?,
        presentation_message_fd: next()?,
        presentation_message_notify_fd: next()?,
        backend_request_fd: next()?,
        backend_request_notify_fd: next()?,
        backend_response_fd: next()?,
        backend_response_notify_fd: next()?,
    })
}

pub fn shared_memory_resource(
    target: &ControlTarget,
    resource_id: u64,
    generation: u64,
) -> Result<SharedMemoryFrame> {
    let response = local_request(
        target,
        &brood_ipc::LocalControlRequest::SharedMemoryResource {
            generation,
            resource: resource_id,
        },
    )?;
    let brood_ipc::LocalControlValue::SharedMemoryResource { app_id } = response.message else {
        bail!("private IPC returned the wrong shared-memory response");
    };
    let mut descriptors = response.descriptors.into_iter();
    let fd = descriptors
        .next()
        .context("shared-memory response omitted its descriptor")?;
    ensure!(
        descriptors.next().is_none(),
        "shared-memory response included extra descriptors"
    );
    let image = brood_ipc::SharedImageConsumer::map(fd.as_fd(), generation)
        .context("map shared-memory frame resource")?;
    Ok(SharedMemoryFrame { app_id, image })
}

pub fn latest_dmabuf_frame(target: &ControlTarget) -> Result<DmabufFrame> {
    let connection = connection(target)?;
    let proxy = BroodControlProxyBlocking::builder(&connection)
        .destination(target.destination.as_str())?
        .path(CONTROL_OBJECT_PATH)?
        .interface(CONTROL_INTERFACE)?
        .build()
        .context("build Brood presentation proxy")?;
    let (
        sequence,
        app_id,
        width,
        height,
        fourcc,
        color_space,
        modifier,
        stride,
        offset,
        y_inverted,
        fd,
    ) = proxy
        .latest_dmabuf_frame()
        .context("request latest hosted DMA-BUF frame")?;
    Ok(DmabufFrame {
        sequence,
        app_id,
        width,
        height,
        fourcc,
        color_space: color_space
            .try_into()
            .context("decode hosted DMA-BUF color space")?,
        modifier,
        stride,
        offset,
        y_inverted,
        fd: fd.into(),
    })
}

pub fn dmabuf_resource(
    target: &ControlTarget,
    resource_id: u64,
    generation: u64,
) -> Result<DmabufFrame> {
    let response = local_request(
        target,
        &brood_ipc::LocalControlRequest::DmabufResource {
            generation,
            resource: resource_id,
        },
    )?;
    let brood_ipc::LocalControlValue::DmabufResource(resource) = response.message else {
        bail!("private IPC returned the wrong DMA-BUF response");
    };
    let mut descriptors = response.descriptors.into_iter();
    let fd = descriptors
        .next()
        .context("DMA-BUF response omitted its descriptor")?;
    ensure!(
        descriptors.next().is_none(),
        "DMA-BUF response included extra descriptors"
    );
    Ok(DmabufFrame {
        sequence: resource.sequence,
        app_id: resource.app_id,
        width: resource.width,
        height: resource.height,
        fourcc: resource.fourcc,
        color_space: resource
            .color_space
            .try_into()
            .context("decode DMA-BUF resource color space")?,
        modifier: resource.modifier,
        stride: resource.stride,
        offset: resource.offset,
        y_inverted: resource.y_inverted,
        fd,
    })
}

pub fn handshake(target: &ControlTarget) -> Result<Handshake> {
    let response = local_request(
        target,
        &brood_ipc::LocalControlRequest::Handshake {
            protocol_version: PRESENTATION_PROTOCOL_VERSION,
        },
    )?;
    let brood_ipc::LocalControlValue::Handshake {
        protocol_version,
        server_pid,
    } = response.message
    else {
        bail!("private IPC returned the wrong handshake response");
    };
    ensure!(
        response.descriptors.is_empty(),
        "handshake returned descriptors"
    );
    Ok(Handshake {
        protocol_version,
        server_pid,
    })
}

pub fn launch_client() -> Result<ExitStatus> {
    let client = sidecar_binary("BROOD_CLIENT_BIN", "brood-client")?;
    Command::new(&client)
        .status()
        .with_context(|| format!("start presentation client {}", client.display()))
}

pub fn spawn_client(target: &ControlTarget) -> Result<Child> {
    let client = sidecar_binary("BROOD_CLIENT_BIN", "brood-client")?;
    let mut command = Command::new(&client);
    command
        .arg("--dbus-destination")
        .arg(&target.destination)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(address) = target.address.as_deref() {
        command.arg("--dbus-address").arg(address);
    }
    command
        .spawn()
        .with_context(|| format!("spawn presentation client {}", client.display()))
}

/// Resolve a packaged companion binary.
///
/// Development builds prefer a sibling binary. Split packages fall back to
/// the wrapper-provided `PATH`.
pub fn sidecar_binary(variable: &str, name: &str) -> Result<PathBuf> {
    if let Some(configured) = env::var_os(variable) {
        return Ok(PathBuf::from(configured));
    }
    let sibling = env::current_exe()
        .context("resolve current executable")?
        .with_file_name(name);
    Ok(sibling_or_path(sibling, name))
}

fn sibling_or_path(sibling: PathBuf, name: &str) -> PathBuf {
    if sibling.is_file() {
        sibling
    } else {
        PathBuf::from(name)
    }
}

pub fn connect_or_spawn(target: &ControlTarget, launch: &ServerLaunch) -> Result<Handshake> {
    if let Ok(handshake) = handshake(target) {
        return Ok(handshake);
    }
    let (log_path, mut child) = spawn_server(target, launch)?;
    for _ in 0..SERVER_START_ATTEMPTS {
        if let Ok(handshake) = handshake(target) {
            return Ok(handshake);
        }
        if let Some(status) = child.try_wait().context("check Brood server startup")? {
            bail!(
                "Brood server exited before handshake with {status}; see {}",
                log_path.display()
            );
        }
        thread::sleep(SERVER_START_INTERVAL);
    }
    bail!(
        "Brood server did not complete its handshake; see {}",
        log_path.display()
    )
}

fn connection(target: &ControlTarget) -> Result<zbus::blocking::Connection> {
    match target.address.as_deref() {
        Some(address) => zbus::blocking::connection::Builder::address(address)
            .with_context(|| format!("parse DBus address {address}"))?
            .build()
            .with_context(|| format!("connect DBus address {address}")),
        None => zbus::blocking::Connection::session().context("connect session DBus"),
    }
}

fn local_request(
    target: &ControlTarget,
    request: &brood_ipc::LocalControlRequest,
) -> Result<brood_ipc::ReceivedPacket<brood_ipc::LocalControlValue>> {
    let path = target.local_ipc_path();
    LOCAL_CONNECTIONS.with(|connections| {
        let mut connections = connections.borrow_mut();
        if !connections.contains_key(&path) {
            let transport = LocalTransport::connect_path(&path)
                .with_context(|| format!("connect private Brood IPC at {}", path.display()))?;
            transport.set_send_timeout(Some(Duration::from_secs(15)))?;
            transport.set_receive_timeout(Some(Duration::from_secs(15)))?;
            connections.insert(path.clone(), transport);
        }
        let transport = connections
            .get(&path)
            .context("private Brood IPC connection disappeared")?;
        let result = (|| {
            transport.send(&request, &[])?;
            let response = transport.receive::<brood_ipc::LocalControlResponse>()?;
            let mut descriptors = response.descriptors;
            let mut message = response.message.result.map_err(anyhow::Error::msg)?;
            if let brood_ipc::LocalControlValue::ControlChunk { bytes, complete } = message {
                ensure!(
                    descriptors.is_empty(),
                    "chunked control response included descriptors"
                );
                let mut output = bytes;
                let mut complete = complete;
                while !complete {
                    let response = transport.receive::<brood_ipc::LocalControlResponse>()?;
                    ensure!(
                        response.descriptors.is_empty(),
                        "chunked control response included descriptors"
                    );
                    match response.message.result.map_err(anyhow::Error::msg)? {
                        brood_ipc::LocalControlValue::ControlChunk {
                            bytes,
                            complete: chunk_complete,
                        } => {
                            output.extend_from_slice(&bytes);
                            complete = chunk_complete;
                        }
                        _ => bail!("private IPC interrupted a chunked control response"),
                    }
                }
                message = brood_ipc::LocalControlValue::Control(
                    String::from_utf8(output)
                        .context("decode chunked private IPC control output")?,
                );
                descriptors = Vec::new();
            }
            Ok(brood_ipc::ReceivedPacket {
                message,
                descriptors,
            })
        })();
        if result.is_err() {
            connections.remove(&path);
        }
        result
    })
}

fn spawn_server(target: &ControlTarget, launch: &ServerLaunch) -> Result<(PathBuf, Child)> {
    let log_dir = server_log_dir();
    fs::create_dir_all(&log_dir).with_context(|| format!("create {}", log_dir.display()))?;
    let log_path = log_dir.join("global.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let log_err = log.try_clone().context("clone Brood server log")?;

    let mut command = Command::new(&launch.executable);
    command
        .arg("--headless")
        .arg("--no-initial-surface-session")
        .arg("--dbus-destination")
        .arg(&target.destination)
        .current_dir(&launch.current_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    if let Some(config) = launch.config.as_deref() {
        command.arg("--config").arg(config);
    }
    detach(&mut command);
    let child = command.spawn().with_context(|| {
        format!(
            "start Brood server {} in {}",
            launch.executable.display(),
            launch.current_directory.display()
        )
    })?;
    Ok((log_path, child))
}

fn detach(command: &mut Command) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
}

fn server_log_dir() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(env::temp_dir)
        .join("brood")
        .join("session-server")
}

#[must_use]
pub fn session_marker_value(marker: &str, key: &str) -> Option<String> {
    marker.split_whitespace().find_map(|field| {
        field
            .strip_prefix(key)
            .and_then(|value| value.strip_prefix('='))
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

#[cfg(test)]
mod control_tests {
    use std::path::{Path, PathBuf};

    use super::{ControlTarget, sibling_or_path};

    #[test]
    fn explicit_bus_address_stabilizes_private_ipc_discovery() {
        let target = ControlTarget::new(
            "org.brood.Test".to_string(),
            Some("unix:path=/tmp/brood-test-bus/bus,guid=1234".to_string()),
        );
        let same_target = target.clone();
        let other_target = ControlTarget::new(
            "org.brood.Other".to_string(),
            Some("unix:path=/tmp/brood-test-bus/bus,guid=1234".to_string()),
        );

        assert_eq!(
            target.local_ipc_path().parent(),
            Some(Path::new("/tmp/brood-test-bus/brood"))
        );
        assert_eq!(target.local_ipc_path(), same_target.local_ipc_path());
        assert_ne!(target.local_ipc_path(), other_target.local_ipc_path());
    }

    #[test]
    fn split_package_sidecar_falls_back_to_wrapper_path() {
        assert_eq!(
            sibling_or_path(
                PathBuf::from("/definitely/missing/brood-client"),
                "brood-client"
            ),
            Path::new("brood-client")
        );
    }
}
