use std::{
    env,
    fs::{self, OpenOptions},
    os::fd::OwnedFd,
    path::PathBuf,
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
    DEFAULT_MANIFOLD_DBUS_NAME, MANIFOLD_SESSION_ENV, MANIFOLDD_DBUS_ADDRESS_ENV,
    MANIFOLDD_DBUS_DESTINATION_ENV,
};

pub const CONTROL_OBJECT_PATH: &str = "/org/manifold/Session";
pub const CONTROL_INTERFACE: &str = "org.manifold.Session.Control";
pub const PRESENTATION_PROTOCOL_VERSION: u32 = 9;
const SERVER_START_ATTEMPTS: usize = 180;
const SERVER_START_INTERVAL: Duration = Duration::from_millis(100);

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

#[proxy(
    interface = "org.manifold.Session.Control",
    default_path = "/org/manifold/Session"
)]
trait ManifoldControl {
    fn command(&self, argv: Vec<String>, json: bool) -> zbus::Result<String>;
    fn latest_dmabuf_frame(&self) -> zbus::Result<DbusDmabufFrame>;
    fn open_presentation_fast_path(
        &self,
    ) -> zbus::Result<(
        u64,
        zbus::zvariant::OwnedFd,
        zbus::zvariant::OwnedFd,
        zbus::zvariant::OwnedFd,
        zbus::zvariant::OwnedFd,
        zbus::zvariant::OwnedFd,
        zbus::zvariant::OwnedFd,
    )>;
    fn dmabuf_resource(&self, resource_id: u64) -> zbus::Result<DbusDmabufFrame>;
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
    pub input_fd: OwnedFd,
    pub input_notify_fd: OwnedFd,
    pub frame_fd: OwnedFd,
    pub frame_notify_fd: OwnedFd,
    pub actor_message_fd: OwnedFd,
    pub actor_message_notify_fd: OwnedFd,
}

#[derive(Debug)]
pub struct DmabufFrame {
    pub sequence: u64,
    pub app_id: String,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub color_space: manifold_ipc::FrameColorSpace,
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
        let executable = env::var_os("MANIFOLDD_BIN").map_or_else(
            || current_executable.with_file_name("manifold-sessiond"),
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

fn presentation_proxy<'a>(
    connection: &'a zbus::blocking::Connection,
    target: &'a ControlTarget,
) -> Result<ManifoldControlProxyBlocking<'a>> {
    ManifoldControlProxyBlocking::builder(connection)
        .destination(target.destination.as_str())?
        .path(CONTROL_OBJECT_PATH)?
        .interface(CONTROL_INTERFACE)?
        .build()
        .context("build Manifold presentation proxy")
}

pub fn open_presentation_fast_path(target: &ControlTarget) -> Result<PresentationFastPath> {
    let connection = connection(target)?;
    let (
        generation,
        input_fd,
        input_notify_fd,
        frame_fd,
        frame_notify_fd,
        actor_message_fd,
        actor_message_notify_fd,
    ) = presentation_proxy(&connection, target)?
        .open_presentation_fast_path()
        .context("open local presentation fast path")?;
    Ok(PresentationFastPath {
        generation,
        input_fd: input_fd.into(),
        input_notify_fd: input_notify_fd.into(),
        frame_fd: frame_fd.into(),
        frame_notify_fd: frame_notify_fd.into(),
        actor_message_fd: actor_message_fd.into(),
        actor_message_notify_fd: actor_message_notify_fd.into(),
    })
}

pub fn latest_dmabuf_frame(target: &ControlTarget) -> Result<DmabufFrame> {
    let connection = connection(target)?;
    let proxy = ManifoldControlProxyBlocking::builder(&connection)
        .destination(target.destination.as_str())?
        .path(CONTROL_OBJECT_PATH)?
        .interface(CONTROL_INTERFACE)?
        .build()
        .context("build Manifold presentation proxy")?;
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

pub fn dmabuf_resource(target: &ControlTarget, resource_id: u64) -> Result<DmabufFrame> {
    let connection = connection(target)?;
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
    ) = presentation_proxy(&connection, target)?
        .dmabuf_resource(resource_id)
        .with_context(|| format!("fetch DMA-BUF resource {resource_id}"))?;
    Ok(DmabufFrame {
        sequence,
        app_id,
        width,
        height,
        fourcc,
        color_space: color_space
            .try_into()
            .context("decode DMA-BUF resource color space")?,
        modifier,
        stride,
        offset,
        y_inverted,
        fd: fd.into(),
    })
}

pub fn handshake(target: &ControlTarget) -> Result<Handshake> {
    let response = request_dbus_command(
        target,
        &[
            "handshake".to_string(),
            PRESENTATION_PROTOCOL_VERSION.to_string(),
        ],
        false,
    )?;
    let handshake: Handshake =
        serde_json::from_str(response.trim()).context("decode presentation handshake")?;
    ensure!(
        handshake.protocol_version == PRESENTATION_PROTOCOL_VERSION,
        "presentation protocol mismatch: client={}, server={}",
        PRESENTATION_PROTOCOL_VERSION,
        handshake.protocol_version
    );
    Ok(handshake)
}

pub fn launch_client() -> Result<ExitStatus> {
    let current_executable = env::current_exe().context("resolve current executable")?;
    let client = env::var_os("MANIFOLD_CLIENT_BIN").map_or_else(
        || current_executable.with_file_name("manifold-client"),
        PathBuf::from,
    );
    Command::new(&client)
        .status()
        .with_context(|| format!("start presentation client {}", client.display()))
}

pub fn connect_or_spawn(target: &ControlTarget, launch: &ServerLaunch) -> Result<Handshake> {
    if let Ok(handshake) = handshake(target) {
        return Ok(handshake);
    }
    ensure!(
        target.destination == DEFAULT_MANIFOLD_DBUS_NAME && target.address.is_none(),
        "cannot auto-start a server for non-default DBus target {}",
        target.destination
    );

    let (log_path, mut child) = spawn_server(target, launch)?;
    for _ in 0..SERVER_START_ATTEMPTS {
        if let Ok(handshake) = handshake(target) {
            return Ok(handshake);
        }
        if let Some(status) = child.try_wait().context("check Manifold server startup")? {
            bail!(
                "Manifold server exited before handshake with {status}; see {}",
                log_path.display()
            );
        }
        thread::sleep(SERVER_START_INTERVAL);
    }
    bail!(
        "Manifold server did not complete its handshake; see {}",
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

fn spawn_server(target: &ControlTarget, launch: &ServerLaunch) -> Result<(PathBuf, Child)> {
    let log_dir = server_log_dir();
    fs::create_dir_all(&log_dir).with_context(|| format!("create {}", log_dir.display()))?;
    let log_path = log_dir.join("global.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let log_err = log.try_clone().context("clone Manifold server log")?;

    let mut command = Command::new(&launch.executable);
    command
        .arg("--offscreen-presentation")
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
            "start Manifold server {} in {}",
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
        .join("manifold")
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
