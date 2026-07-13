use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::session_bus_state::{
    BusRouter, ConnectionId, MessageClass, NameOwnerChange, NotificationForwardDecision,
    NotificationProxyTransition, NotificationSignal, RouteDecision, route_notification_signal,
};

use anyhow::{Context, Result, anyhow};
use tokio::{runtime::Handle as TokioHandle, task::JoinHandle};
use tracing::{debug, warn};
use zbus::{
    Message,
    blocking::{Proxy, connection::Builder as ConnectionBuilder},
    message::Type,
    zvariant::{
        Endian, OwnedValue,
        serialized::{Context as SerializedContext, Data},
    },
};

pub const DBUS_DESTINATION: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";
pub const NOTIFICATIONS_DESTINATION: &str = "org.freedesktop.Notifications";
pub const NOTIFICATIONS_UNIQUE_NAME: &str = ":1.999999";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const NOTIFICATIONS_INTERFACE: &str = "org.freedesktop.Notifications";
const DBUS_PRIMARY_HEADER_LEN: usize = 16;
const MAX_DBUS_FRAME_LEN: usize = 128 * 1024 * 1024;
const INTROSPECTABLE_INTERFACE: &str = "org.freedesktop.DBus.Introspectable";
const NOTIFICATIONS_INTROSPECTION_XML: &str = r#"<node>
  <interface name="org.freedesktop.Notifications">
    <method name="GetCapabilities">
      <arg direction="out" type="as"/>
    </method>
    <method name="Notify">
      <arg direction="in" type="s"/>
      <arg direction="in" type="u"/>
      <arg direction="in" type="s"/>
      <arg direction="in" type="s"/>
      <arg direction="in" type="s"/>
      <arg direction="in" type="as"/>
      <arg direction="in" type="a{sv}"/>
      <arg direction="in" type="i"/>
      <arg direction="out" type="u"/>
    </method>
    <method name="CloseNotification">
      <arg direction="in" type="u"/>
    </method>
    <method name="GetServerInformation">
      <arg direction="out" type="s"/>
      <arg direction="out" type="s"/>
      <arg direction="out" type="s"/>
      <arg direction="out" type="s"/>
    </method>
    <signal name="NotificationClosed">
      <arg type="u"/>
      <arg type="u"/>
    </signal>
    <signal name="ActionInvoked">
      <arg type="u"/>
      <arg type="s"/>
    </signal>
    <signal name="ActivationToken">
      <arg type="u"/>
      <arg type="s"/>
    </signal>
  </interface>
  <interface name="org.freedesktop.DBus.Introspectable">
    <method name="Introspect">
      <arg direction="out" type="s"/>
    </method>
  </interface>
</node>"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionBusRealm {
    Surface(u64),
    #[allow(dead_code, reason = "reserved for non-surface managed realms")]
    Headless(u64),
}

pub struct SessionBusHandle {
    address: String,
    socket_path: PathBuf,
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    task: Option<JoinHandle<()>>,
}

impl SessionBusHandle {
    pub(crate) fn start(realm: SessionBusRealm, host_bus_address: Option<String>) -> Result<Self> {
        let StartedSessionBus {
            address,
            socket_path,
            dir,
            listener,
            state,
            stop,
        } = Self::bind(realm, host_bus_address)?;
        let thread_stop = Arc::clone(&stop);
        let thread_state = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name(format!("manifold-dbus-{}", realm.label()))
            .spawn(move || accept_loop(&listener, &thread_state, &thread_stop))
            .context("spawn in-process DBus realm thread")?;

        Ok(Self {
            address,
            socket_path,
            dir,
            stop,
            thread: Some(thread),
            task: None,
        })
    }

    pub(crate) fn start_on_runtime(
        realm: SessionBusRealm,
        host_bus_address: Option<String>,
        runtime: &TokioHandle,
    ) -> Result<Self> {
        let StartedSessionBus {
            address,
            socket_path,
            dir,
            listener,
            state,
            stop,
        } = Self::bind(realm, host_bus_address)?;
        let task_stop = Arc::clone(&stop);
        let task_state = Arc::clone(&state);
        let task_runtime = runtime.clone();
        let task = runtime.spawn_blocking(move || {
            accept_loop_on_runtime(&listener, &task_state, &task_stop, &task_runtime);
        });

        Ok(Self {
            address,
            socket_path,
            dir,
            stop,
            thread: None,
            task: Some(task),
        })
    }

    fn bind(realm: SessionBusRealm, host_bus_address: Option<String>) -> Result<StartedSessionBus> {
        let dir = std::env::temp_dir().join(format!(
            "manifold-inprocess-dbus-{}-{}",
            std::process::id(),
            realm.label()
        ));
        if let Err(error) = fs::remove_dir_all(&dir)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(error).with_context(|| format!("remove stale {}", dir.display()));
        }
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let socket_path = dir.join("bus");
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind {}", socket_path.display()))?;
        listener
            .set_nonblocking(true)
            .context("set DBus realm listener nonblocking")?;

        let guid = make_guid(realm);
        let address = format!("unix:path={}", socket_path.display());
        let state = Arc::new(Mutex::new(RealmState::new(realm, guid, host_bus_address)));
        let stop = Arc::new(AtomicBool::new(false));

        Ok(StartedSessionBus {
            address,
            socket_path,
            dir,
            listener,
            state,
            stop,
        })
    }

    pub(crate) fn address(&self) -> &str {
        &self.address
    }
}

impl Drop for SessionBusHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Err(error) = UnixStream::connect(&self.socket_path) {
            debug!(?error, path = %self.socket_path.display(), "session bus stop wakeup failed");
        }
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            warn!("in-process DBus realm thread panicked during shutdown");
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Err(error) = fs::remove_dir_all(&self.dir)
            && error.kind() != io::ErrorKind::NotFound
        {
            warn!(?error, path = %self.dir.display(), "remove session bus directory failed");
        }
    }
}

struct StartedSessionBus {
    address: String,
    socket_path: PathBuf,
    dir: PathBuf,
    listener: UnixListener,
    state: Arc<Mutex<RealmState>>,
    stop: Arc<AtomicBool>,
}

struct RealmState {
    realm: SessionBusRealm,
    guid: String,
    host_bus_address: Option<String>,
    router: BusRouter,
}

impl RealmState {
    const fn new(realm: SessionBusRealm, guid: String, host_bus_address: Option<String>) -> Self {
        Self {
            realm,
            guid,
            router: BusRouter::new(host_bus_address.is_some()),
            host_bus_address,
        }
    }
}

fn accept_loop(listener: &UnixListener, state: &Arc<Mutex<RealmState>>, stop: &Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let connection_state = Arc::clone(state);
                match thread::Builder::new()
                    .name("manifold-dbus-client".to_string())
                    .spawn(move || {
                        if let Err(err) = handle_connection(stream, &connection_state) {
                            debug!(?err, "in-process DBus client disconnected");
                        }
                    }) {
                    Ok(handle) => drop(handle),
                    Err(error) => warn!(?error, "spawn in-process DBus client failed"),
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) => {
                warn!(?err, "in-process DBus realm accept failed");
                break;
            }
        }
    }
}

fn accept_loop_on_runtime(
    listener: &UnixListener,
    state: &Arc<Mutex<RealmState>>,
    stop: &Arc<AtomicBool>,
    runtime: &TokioHandle,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let connection_state = Arc::clone(state);
                let task = runtime.spawn_blocking(move || {
                    if let Err(err) = handle_connection(stream, &connection_state) {
                        debug!(?err, "in-process DBus client disconnected");
                    }
                });
                drop(task);
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) => {
                warn!(?err, "in-process DBus realm accept failed");
                break;
            }
        }
    }
}

fn handle_connection(stream: UnixStream, state: &Arc<Mutex<RealmState>>) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone().context("clone DBus client stream")?);
    let guid = state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?
        .guid
        .clone();
    authenticate(&mut reader, &guid)?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let connection_id = state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?
        .router
        .connect(tx)?;

    let mut writer = stream.try_clone().context("clone DBus writer stream")?;
    let writer_thread = thread::Builder::new()
        .name("manifold-dbus-writer".to_string())
        .spawn(move || {
            for bytes in rx {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
            }
        })
        .context("spawn DBus writer")?;

    let result = read_messages(&mut reader, connection_id, state);
    let cleanup = disconnect_connection(connection_id, state);
    drop(stream);
    writer_thread
        .join()
        .map_err(|_panic_payload| anyhow!("DBus writer thread panicked"))?;
    result.and(cleanup)
}

fn disconnect_connection(
    connection_id: ConnectionId,
    state: &Arc<Mutex<RealmState>>,
) -> Result<()> {
    let mut state = state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?;
    let transition = state.router.disconnect(connection_id)?;
    for owner_change in transition.owner_changes {
        emit_name_owner_changed(&state, &owner_change)?;
    }
    drop(state);
    Ok(())
}

fn authenticate(reader: &mut BufReader<UnixStream>, guid: &str) -> Result<()> {
    let mut initial = [0; 1];
    reader
        .get_mut()
        .read_exact(&mut initial)
        .context("read DBus auth nul")?;
    if initial != [0] {
        return Err(anyhow!("unsupported DBus auth prelude"));
    }

    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).context("read DBus auth line")?;
        if read == 0 {
            return Err(anyhow!("DBus auth stream closed"));
        }
        let command = line.trim_end_matches(['\r', '\n']);
        if command == "AUTH" {
            reader
                .get_mut()
                .write_all(b"REJECTED EXTERNAL ANONYMOUS\r\n")
                .context("write DBus auth mechanisms")?;
        } else if command == "AUTH EXTERNAL" {
            reader
                .get_mut()
                .write_all(b"DATA\r\n")
                .context("write DBus external auth data challenge")?;
        } else if command.starts_with("DATA") {
            reader
                .get_mut()
                .write_all(format!("OK {guid}\r\n").as_bytes())
                .context("write DBus auth data ok")?;
        } else if command.starts_with("AUTH EXTERNAL ") || command.starts_with("AUTH ANONYMOUS") {
            reader
                .get_mut()
                .write_all(format!("OK {guid}\r\n").as_bytes())
                .context("write DBus auth ok")?;
        } else if command.starts_with("AUTH ") {
            reader
                .get_mut()
                .write_all(b"REJECTED EXTERNAL ANONYMOUS\r\n")
                .context("write DBus auth rejected mechanisms")?;
        } else if command == "NEGOTIATE_UNIX_FD" {
            reader
                .get_mut()
                .write_all(b"ERROR unix-fd-passing-not-supported\r\n")
                .context("write DBus unix fd negotiation response")?;
        } else if command == "BEGIN" {
            return Ok(());
        } else if command == "CANCEL" {
            reader
                .get_mut()
                .write_all(b"REJECTED EXTERNAL ANONYMOUS\r\n")
                .context("write DBus auth rejected")?;
        } else {
            reader
                .get_mut()
                .write_all(b"ERROR unsupported-auth-command\r\n")
                .context("write DBus auth error")?;
        }
    }
}

fn read_messages(
    reader: &mut BufReader<UnixStream>,
    connection_id: ConnectionId,
    state: &Arc<Mutex<RealmState>>,
) -> Result<()> {
    loop {
        let bytes = read_message_bytes(reader)?;
        let message = parse_message(bytes.clone()).context("parse DBus message")?;
        route_message(connection_id, &message, bytes, state)?;
    }
}

fn read_message_bytes(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut primary = [0; DBUS_PRIMARY_HEADER_LEN];
    reader
        .read_exact(&mut primary)
        .context("read DBus primary header")?;
    let header = DbusFrameHeader::decode(primary)?;
    let rest_len = header.remaining_len()?;
    let mut bytes = primary.to_vec();
    bytes.resize(header.total_len()?, 0);
    reader
        .read_exact(
            bytes
                .get_mut(DBUS_PRIMARY_HEADER_LEN..)
                .ok_or_else(|| anyhow!("invalid DBus frame storage"))?,
        )
        .context("read DBus message body")?;
    debug_assert_eq!(rest_len, bytes.len() - DBUS_PRIMARY_HEADER_LEN);
    Ok(bytes)
}

/// Checked shape of a D-Bus wire frame before any allocation or zbus parsing.
///
/// D-Bus fixes the primary header at 16 bytes and aligns the body to eight
/// bytes after its variable header fields. This type owns the endian decision
/// and all length arithmetic so socket input cannot trigger unchecked slicing,
/// integer wrapping, or an unbounded allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DbusFrameHeader {
    endian: Endian,
    body_len: usize,
    fields_len: usize,
}

impl DbusFrameHeader {
    fn decode(primary: [u8; DBUS_PRIMARY_HEADER_LEN]) -> Result<Self> {
        let endian = match primary.first().copied() {
            Some(b'l') => Endian::Little,
            Some(b'B') => Endian::Big,
            Some(other) => return Err(anyhow!("unsupported DBus endian marker {other}")),
            None => return Err(anyhow!("missing DBus endian marker")),
        };
        let body_len = usize::try_from(read_u32(
            primary
                .get(4..8)
                .ok_or_else(|| anyhow!("missing DBus body length"))?,
            endian,
        )?)?;
        let fields_len = usize::try_from(read_u32(
            primary
                .get(12..16)
                .ok_or_else(|| anyhow!("missing DBus fields length"))?,
            endian,
        )?)?;
        Ok(Self {
            endian,
            body_len,
            fields_len,
        })
    }

    fn remaining_len(self) -> Result<usize> {
        let fields_end = DBUS_PRIMARY_HEADER_LEN
            .checked_add(self.fields_len)
            .ok_or_else(|| anyhow!("DBus fields length overflow"))?;
        self.fields_len
            .checked_add(padding_for_8_bytes(fields_end))
            .and_then(|header| header.checked_add(self.body_len))
            .filter(|len| *len <= MAX_DBUS_FRAME_LEN - DBUS_PRIMARY_HEADER_LEN)
            .ok_or_else(|| anyhow!("DBus frame exceeds {MAX_DBUS_FRAME_LEN} bytes"))
    }

    fn total_len(self) -> Result<usize> {
        DBUS_PRIMARY_HEADER_LEN
            .checked_add(self.remaining_len()?)
            .ok_or_else(|| anyhow!("DBus frame length overflow"))
    }
}

fn route_message(
    connection_id: ConnectionId,
    message: &Message,
    original_bytes: Vec<u8>,
    state: &Arc<Mutex<RealmState>>,
) -> Result<()> {
    let header = message.header();
    let destination = header.destination().map(ToString::to_string);
    let interface = header.interface().map(ToString::to_string);
    let path = header.path().map(ToString::to_string);
    let member = header.member().map(ToString::to_string);

    if header.message_type() == Type::MethodCall
        && destination.as_deref() == Some(DBUS_DESTINATION)
        && path.as_deref() == Some(DBUS_PATH)
        && interface.as_deref() == Some(DBUS_INTERFACE)
    {
        return handle_dbus_method(connection_id, message, member.as_deref(), state);
    }

    if header.message_type() == Type::MethodCall
        && is_notifications_destination(destination.as_deref())
        && path.as_deref() == Some(NOTIFICATIONS_PATH)
        && matches!(
            interface.as_deref(),
            Some(NOTIFICATIONS_INTERFACE | INTROSPECTABLE_INTERFACE)
        )
    {
        return handle_notifications_method(connection_id, message, member.as_deref(), state);
    }

    let unique = ensure_unique(connection_id, state)?;
    let outbound = match header.message_type() {
        Type::MethodCall => inject_sender(message, &unique)?,
        Type::Signal => inject_signal_sender(message, &unique)?,
        Type::MethodReturn | Type::Error => original_bytes,
    };

    let state = state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?;
    let class = match header.message_type() {
        Type::MethodCall => MessageClass::MethodCall,
        Type::MethodReturn => MessageClass::MethodReturn,
        Type::Error => MessageClass::Error,
        Type::Signal => MessageClass::Signal,
    };
    match state
        .router
        .route(connection_id, class, destination.as_deref())?
    {
        RouteDecision::Unicast(recipient) => state.router.deliver(recipient, outbound)?,
        RouteDecision::Broadcast(recipients) => {
            for recipient in recipients {
                state.router.deliver(recipient, outbound.clone())?;
            }
        }
        RouteDecision::UnknownDestination => send_error(
            &state,
            connection_id,
            message,
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "destination has no owner",
        )?,
        RouteDecision::Drop => {}
    }
    drop(state);
    Ok(())
}

fn handle_dbus_method(
    connection_id: ConnectionId,
    message: &Message,
    member: Option<&str>,
    state: &Arc<Mutex<RealmState>>,
) -> Result<()> {
    let mut state = state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?;
    match member {
        Some("Hello") => {
            let name = state.router.ensure_unique(connection_id)?;
            send_reply(&state, connection_id, message, &(name.as_str(),))?;
        }
        Some("RequestName") => {
            let (name, flags): (String, u32) = message.body().deserialize()?;
            match state.router.request_name(connection_id, name, flags) {
                Ok(transition) => {
                    send_reply(
                        &state,
                        connection_id,
                        message,
                        &(transition.outcome.reply_code(),),
                    )?;
                    if let Some(owner_change) = transition.owner_change {
                        emit_name_owner_changed(&state, &owner_change)?;
                    }
                }
                Err(error) => {
                    send_error(
                        &state,
                        connection_id,
                        message,
                        "org.freedesktop.DBus.Error.InvalidArgs",
                        &error.to_string(),
                    )?;
                }
            }
        }
        Some("ReleaseName") => {
            let (name,): (String,) = message.body().deserialize()?;
            let transition = state.router.release_name(connection_id, &name)?;
            send_reply(
                &state,
                connection_id,
                message,
                &(transition.outcome.reply_code(),),
            )?;
            if let Some(owner_change) = transition.owner_change {
                emit_name_owner_changed(&state, &owner_change)?;
            }
        }
        Some("NameHasOwner") => {
            let (name,): (String,) = message.body().deserialize()?;
            let has_owner = state.router.name_owner(&name).is_some();
            send_reply(&state, connection_id, message, &(has_owner,))?;
        }
        Some("GetNameOwner") => {
            let (name,): (String,) = message.body().deserialize()?;
            if let Some(owner_name) = state.router.owner_unique_name(&name) {
                send_reply(&state, connection_id, message, &(owner_name,))?;
            } else {
                send_error(
                    &state,
                    connection_id,
                    message,
                    "org.freedesktop.DBus.Error.NameHasNoOwner",
                    "name has no owner",
                )?;
            }
        }
        Some("StartServiceByName") => {
            let (name, _flags): (String, u32) = message.body().deserialize()?;
            if name == NOTIFICATIONS_DESTINATION && state.host_bus_address.is_some() {
                send_reply(&state, connection_id, message, &(2u32,))?;
            } else {
                send_error(
                    &state,
                    connection_id,
                    message,
                    "org.freedesktop.DBus.Error.ServiceUnknown",
                    "service activation is unavailable",
                )?;
            }
        }
        Some("ListNames") => {
            let names = state.router.list_names();
            send_reply(&state, connection_id, message, &(names,))?;
        }
        Some("AddMatch" | "RemoveMatch") => {
            send_reply(&state, connection_id, message, &())?;
        }
        Some("GetId") => {
            let guid = state.guid.clone();
            send_reply(&state, connection_id, message, &(guid,))?;
        }
        Some(other) => {
            warn!(
                ?other,
                realm = ?state.realm,
                "unsupported in-process DBus daemon method"
            );
            send_error(
                &state,
                connection_id,
                message,
                "org.freedesktop.DBus.Error.UnknownMethod",
                "unsupported bus method",
            )?;
        }
        None => {
            send_error(
                &state,
                connection_id,
                message,
                "org.freedesktop.DBus.Error.UnknownMethod",
                "missing bus method",
            )?;
        }
    }
    Ok(())
}

enum NotificationMethodReply {
    Unit,
    Capabilities(Vec<String>),
    Introspection(&'static str),
    ServerInformation((String, String, String, String)),
    NotificationId(u32),
}

type HostNotifyArguments = (
    String,
    u32,
    String,
    String,
    String,
    Vec<String>,
    BTreeMap<String, OwnedValue>,
    i32,
);

fn handle_notifications_method(
    connection_id: ConnectionId,
    message: &Message,
    member: Option<&str>,
    state: &Arc<Mutex<RealmState>>,
) -> Result<()> {
    let host_bus_address = state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?
        .host_bus_address
        .clone();
    let Some(host_bus_address) = host_bus_address else {
        let state = state
            .lock()
            .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?;
        return send_error(
            &state,
            connection_id,
            message,
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "notification service is unavailable",
        );
    };

    let reply = match NotificationProxyTransition::from_result(call_host_notifications_method(
        &host_bus_address,
        message,
        member,
    )) {
        NotificationProxyTransition::Succeeded(reply) => reply,
        NotificationProxyTransition::Failed(error) => {
            warn!(
                %error,
                ?member,
                "failed to proxy notification method to host session bus"
            );
            let state = state
                .lock()
                .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?;
            return send_error(
                &state,
                connection_id,
                message,
                "org.freedesktop.DBus.Error.Failed",
                &error,
            );
        }
    };

    let state_guard = state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?;
    match reply {
        NotificationMethodReply::Unit => send_reply_from(
            &state_guard,
            connection_id,
            message,
            NOTIFICATIONS_UNIQUE_NAME,
            &(),
        ),
        NotificationMethodReply::Capabilities(capabilities) => send_reply_from(
            &state_guard,
            connection_id,
            message,
            NOTIFICATIONS_UNIQUE_NAME,
            &(capabilities,),
        ),
        NotificationMethodReply::Introspection(xml) => send_reply_from(
            &state_guard,
            connection_id,
            message,
            NOTIFICATIONS_UNIQUE_NAME,
            &(xml,),
        ),
        NotificationMethodReply::ServerInformation(info) => send_reply_from(
            &state_guard,
            connection_id,
            message,
            NOTIFICATIONS_UNIQUE_NAME,
            &info,
        ),
        NotificationMethodReply::NotificationId(notification_id) => {
            send_reply_from(
                &state_guard,
                connection_id,
                message,
                NOTIFICATIONS_UNIQUE_NAME,
                &(notification_id,),
            )?;
            drop(state_guard);
            spawn_notification_signal_forwarder(
                host_bus_address,
                Arc::clone(state),
                connection_id,
                notification_id,
            );
            Ok(())
        }
    }
}

fn call_host_notifications_method(
    host_bus_address: &str,
    message: &Message,
    member: Option<&str>,
) -> Result<NotificationMethodReply> {
    let connection = ConnectionBuilder::address(host_bus_address)?
        .build()
        .context("connect host session bus")?;
    let proxy = Proxy::new(
        &connection,
        NOTIFICATIONS_DESTINATION,
        NOTIFICATIONS_PATH,
        NOTIFICATIONS_INTERFACE,
    )
    .context("build host notification proxy")?;

    match member {
        Some("Introspect") => Ok(NotificationMethodReply::Introspection(
            NOTIFICATIONS_INTROSPECTION_XML,
        )),
        Some("GetCapabilities") => {
            let capabilities: Vec<String> = proxy
                .call("GetCapabilities", &())
                .context("call host notification GetCapabilities")?;
            Ok(NotificationMethodReply::Capabilities(capabilities))
        }
        Some("GetServerInformation") => {
            let info: (String, String, String, String) = proxy
                .call("GetServerInformation", &())
                .context("call host notification GetServerInformation")?;
            Ok(NotificationMethodReply::ServerInformation(info))
        }
        Some("CloseNotification") => {
            let (notification_id,): (u32,) = message.body().deserialize()?;
            proxy
                .call::<_, _, ()>("CloseNotification", &(notification_id,))
                .context("call host notification CloseNotification")?;
            Ok(NotificationMethodReply::Unit)
        }
        Some("Notify") => {
            let (app_name, replaces_id, app_icon, summary, body, actions, hints, expire_timeout):
                HostNotifyArguments = message.body().deserialize()?;
            let notification_id: u32 = proxy
                .call(
                    "Notify",
                    &(
                        app_name,
                        replaces_id,
                        app_icon,
                        summary,
                        body,
                        actions,
                        hints,
                        expire_timeout,
                    ),
                )
                .context("call host notification Notify")?;
            Ok(NotificationMethodReply::NotificationId(notification_id))
        }
        Some(other) => Err(anyhow!("unsupported notification method {other}")),
        None => Err(anyhow!("missing notification method")),
    }
}

fn is_notifications_destination(destination: Option<&str>) -> bool {
    matches!(
        destination,
        Some(NOTIFICATIONS_DESTINATION | NOTIFICATIONS_UNIQUE_NAME)
    )
}

fn spawn_notification_signal_forwarder(
    host_bus_address: String,
    state: Arc<Mutex<RealmState>>,
    connection_id: ConnectionId,
    notification_id: u32,
) {
    let result = thread::Builder::new()
        .name(format!("manifold-notify-{notification_id}"))
        .spawn(move || {
            if let Err(err) = forward_notification_signals(
                &host_bus_address,
                &state,
                connection_id,
                notification_id,
            ) {
                debug!(
                    ?err,
                    notification_id, "host notification signal forwarding stopped"
                );
            }
        });
    if let Err(err) = result {
        warn!(
            ?err,
            notification_id, "failed to spawn host notification signal forwarder"
        );
    }
}

fn forward_notification_signals(
    host_bus_address: &str,
    state: &Arc<Mutex<RealmState>>,
    connection_id: ConnectionId,
    notification_id: u32,
) -> Result<()> {
    let connection = ConnectionBuilder::address(host_bus_address)?
        .build()
        .context("connect host session bus for notification signals")?;
    let proxy = Proxy::new(
        &connection,
        NOTIFICATIONS_DESTINATION,
        NOTIFICATIONS_PATH,
        NOTIFICATIONS_INTERFACE,
    )
    .context("build host notification signal proxy")?;
    let mut signals = proxy
        .receive_all_signals()
        .context("subscribe to host notification signals")?;

    for signal in &mut signals {
        let member = signal.header().member().map(ToString::to_string);
        let notification_signal = match member.as_deref() {
            Some("ActionInvoked") => {
                let (id, action_key): (u32, String) = signal.body().deserialize()?;
                Some(NotificationSignal::ActionInvoked { id, action_key })
            }
            Some("ActivationToken") => {
                let (id, token): (u32, String) = signal.body().deserialize()?;
                Some(NotificationSignal::ActivationToken { id, token })
            }
            Some("NotificationClosed") => {
                let (id, reason): (u32, u32) = signal.body().deserialize()?;
                Some(NotificationSignal::Closed { id, reason })
            }
            _ => None,
        };
        if let Some(notification_signal) = notification_signal
            && let NotificationForwardDecision::Deliver(notification_signal) =
                route_notification_signal(notification_id, notification_signal)
        {
            let closes_forwarder = notification_signal.closes_forwarder();
            send_notification_signal(state, connection_id, &notification_signal)?;
            if closes_forwarder {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn send_notification_signal(
    state: &Arc<Mutex<RealmState>>,
    connection_id: ConnectionId,
    notification: &NotificationSignal,
) -> Result<()> {
    let builder = Message::signal(
        NOTIFICATIONS_PATH,
        NOTIFICATIONS_INTERFACE,
        match notification {
            NotificationSignal::ActionInvoked { .. } => "ActionInvoked",
            NotificationSignal::ActivationToken { .. } => "ActivationToken",
            NotificationSignal::Closed { .. } => "NotificationClosed",
        },
    )?
    .sender(NOTIFICATIONS_UNIQUE_NAME)?;
    let signal = match notification {
        NotificationSignal::ActionInvoked { id, action_key } => {
            builder.build(&(*id, action_key.as_str()))?
        }
        NotificationSignal::ActivationToken { id, token } => {
            builder.build(&(*id, token.as_str()))?
        }
        NotificationSignal::Closed { id, reason } => builder.build(&(*id, *reason))?,
    };
    let state = state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?;
    state.router.deliver(connection_id, signal.data().to_vec())
}

fn emit_name_owner_changed(state: &RealmState, change: &NameOwnerChange) -> Result<()> {
    let signal = Message::signal(DBUS_PATH, DBUS_INTERFACE, "NameOwnerChanged")?
        .sender(":1.0")?
        .build(&(
            change.name.as_str(),
            change.old_owner.as_str(),
            change.new_owner.as_str(),
        ))?;
    state.router.broadcast(signal.data(), None)
}

fn send_reply<B>(
    state: &RealmState,
    connection_id: ConnectionId,
    message: &Message,
    body: &B,
) -> Result<()>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    send_reply_from(state, connection_id, message, ":1.0", body)
}

fn send_reply_from<B>(
    state: &RealmState,
    connection_id: ConnectionId,
    message: &Message,
    sender: &str,
    body: &B,
) -> Result<()>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let reply = Message::method_return(&message.header())?
        .sender(sender)?
        .build(body)?;
    state.router.deliver(connection_id, reply.data().to_vec())
}

fn send_error(
    state: &RealmState,
    connection_id: ConnectionId,
    message: &Message,
    name: &str,
    text: &str,
) -> Result<()> {
    let reply = Message::error(&message.header(), name)?
        .sender(":1.0")?
        .build(&(text,))?;
    state.router.deliver(connection_id, reply.data().to_vec())
}

fn ensure_unique(connection_id: ConnectionId, state: &Arc<Mutex<RealmState>>) -> Result<String> {
    Ok(state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?
        .router
        .ensure_unique(connection_id)?
        .as_str()
        .to_string())
}

fn inject_sender(message: &Message, sender: &str) -> Result<Vec<u8>> {
    let header = message.header();
    let path = header
        .path()
        .ok_or_else(|| anyhow!("method call missing path"))?;
    let member = header
        .member()
        .ok_or_else(|| anyhow!("method call missing member"))?;
    let mut builder = Message::method_call(path.to_string(), member.to_string())?
        .sender(sender)?
        .serial(header.primary().serial_num())
        .endian(header.primary().endian_sig().into());
    if let Some(interface) = header.interface() {
        builder = builder.interface(interface.to_string())?;
    }
    if let Some(destination) = header.destination() {
        builder = builder.destination(destination.to_string())?;
    }
    build_raw_like(builder, message)
}

fn inject_signal_sender(message: &Message, sender: &str) -> Result<Vec<u8>> {
    let header = message.header();
    let path = header
        .path()
        .ok_or_else(|| anyhow!("signal missing path"))?;
    let interface = header
        .interface()
        .ok_or_else(|| anyhow!("signal missing interface"))?;
    let member = header
        .member()
        .ok_or_else(|| anyhow!("signal missing member"))?;
    let mut builder = Message::signal(path.to_string(), interface.to_string(), member.to_string())?
        .sender(sender)?
        .serial(header.primary().serial_num())
        .endian(header.primary().endian_sig().into());
    if let Some(destination) = header.destination() {
        builder = builder.destination(destination.to_string())?;
    }
    build_raw_like(builder, message)
}

fn build_raw_like(builder: zbus::message::Builder<'_>, message: &Message) -> Result<Vec<u8>> {
    let body = message.body();
    let signature = body.signature().to_owned();
    let body_bytes = body.data().to_vec();
    let rebuilt = build_validated_raw_body(builder, &body_bytes, signature)?;
    Ok(rebuilt.data().to_vec())
}

/// Rebuild a body which zbus has already validated, changing only bus-owned headers.
fn build_validated_raw_body(
    builder: zbus::message::Builder<'_>,
    body_bytes: &[u8],
    signature: zbus::zvariant::Signature,
) -> Result<Message> {
    // SAFETY: `body_bytes` and `signature` come from the same successfully
    // parsed zbus `Message::body`. They therefore agree on layout, alignment,
    // endianness, and lifetimes; no FDs are present because this broker rejects
    // Unix-FD negotiation.
    Ok(unsafe { builder.build_raw_body(body_bytes, signature, Vec::new())? })
}

fn parse_message(bytes: Vec<u8>) -> Result<Message> {
    let primary: [u8; DBUS_PRIMARY_HEADER_LEN] = bytes
        .get(..DBUS_PRIMARY_HEADER_LEN)
        .ok_or_else(|| anyhow!("short DBus primary header"))?
        .try_into()
        .map_err(|_short_header| anyhow!("short DBus primary header"))?;
    let header = DbusFrameHeader::decode(primary)?;
    if bytes.len() != header.total_len()? {
        return Err(anyhow!(
            "DBus frame length mismatch: expected {}, received {}",
            header.total_len()?,
            bytes.len()
        ));
    }
    parse_validated_message(bytes, header.endian)
}

/// Hand a length-checked, aligned-at-zero D-Bus frame to zbus.
fn parse_validated_message(bytes: Vec<u8>, endian: Endian) -> Result<Message> {
    let data = Data::new(bytes, SerializedContext::new_dbus(endian, 0));
    // SAFETY: `DbusFrameHeader` validated the fixed header, endian marker,
    // eight-byte body alignment, and exact complete frame length. zbus owns the
    // byte allocation and performs the remaining header/signature validation.
    Ok(unsafe { Message::from_bytes(data)? })
}

fn read_u32(bytes: &[u8], endian: Endian) -> Result<u32> {
    let array: [u8; 4] = bytes
        .try_into()
        .map_err(|_wrong_length| anyhow!("DBus u32 field must contain exactly four bytes"))?;
    Ok(match endian {
        Endian::Little => u32::from_le_bytes(array),
        Endian::Big => u32::from_be_bytes(array),
    })
}

const fn padding_for_8_bytes(len: usize) -> usize {
    (8 - (len % 8)) % 8
}

fn make_guid(realm: SessionBusRealm) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let realm_id = match realm {
        SessionBusRealm::Surface(id) | SessionBusRealm::Headless(id) => id,
    };
    let now_low = u64::try_from(now & u128::from(u64::MAX)).unwrap_or_default();
    let realm_low = u32::try_from(realm_id % (u64::from(u32::MAX) + 1)).unwrap_or_default();
    format!(
        "{:016x}{:08x}{:08x}",
        now_low,
        std::process::id(),
        realm_low
    )
}

impl SessionBusRealm {
    fn label(self) -> String {
        match self {
            Self::Surface(id) => format!("surface-{id}"),
            Self::Headless(id) => format!("headless-{id}"),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::default_trait_access,
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests rely on concise fixed-size frame fixtures and inferred zbus defaults"
)]
mod tests {
    use super::{
        DBUS_PRIMARY_HEADER_LEN, DbusFrameHeader, MAX_DBUS_FRAME_LEN, NOTIFICATIONS_DESTINATION,
        NOTIFICATIONS_UNIQUE_NAME, SessionBusHandle, SessionBusRealm, parse_message,
        read_message_bytes,
    };
    use std::io::Cursor;
    use zbus::{
        blocking::{connection::Builder as ConnectionBuilder, fdo::DBusProxy},
        fdo::RequestNameReply,
        names::{BusName, WellKnownName},
    };

    #[test]
    fn realm_bus_tracks_names_on_in_process_socket() {
        let bus = SessionBusHandle::start(SessionBusRealm::Surface(7), None).unwrap();
        assert!(bus.address().starts_with("unix:path="));
        assert!(!bus.address().contains("tmpdir="));

        let connection = ConnectionBuilder::address(bus.address())
            .unwrap()
            .build()
            .unwrap();
        let proxy = DBusProxy::new(&connection).unwrap();
        let unique = proxy.hello().unwrap();
        assert!(unique.as_str().starts_with(":1."));
        let name = BusName::try_from("org.manifold.Test.SessionBus").unwrap();
        assert!(!proxy.name_has_owner(name.clone()).unwrap());

        let well_known = WellKnownName::try_from("org.manifold.Test.SessionBus").unwrap();
        let reply = proxy.request_name(well_known, Default::default()).unwrap();
        assert_eq!(reply, RequestNameReply::PrimaryOwner);
        assert!(proxy.name_has_owner(name).unwrap());
    }

    #[test]
    fn separate_realms_do_not_share_well_known_names() {
        let first = SessionBusHandle::start(SessionBusRealm::Surface(11), None).unwrap();
        let second = SessionBusHandle::start(SessionBusRealm::Surface(12), None).unwrap();

        let first_connection = ConnectionBuilder::address(first.address())
            .unwrap()
            .build()
            .unwrap();
        let first_proxy = DBusProxy::new(&first_connection).unwrap();
        first_proxy.hello().unwrap();
        let first_name = WellKnownName::try_from("org.manifold.Test.SessionScoped").unwrap();
        first_proxy
            .request_name(first_name, Default::default())
            .unwrap();

        let second_connection = ConnectionBuilder::address(second.address())
            .unwrap()
            .build()
            .unwrap();
        let second_proxy = DBusProxy::new(&second_connection).unwrap();
        second_proxy.hello().unwrap();
        let second_name = BusName::try_from("org.manifold.Test.SessionScoped").unwrap();
        assert!(!second_proxy.name_has_owner(second_name).unwrap());
    }

    #[test]
    fn realm_bus_advertises_virtual_notifications_owner() {
        let bus = SessionBusHandle::start(
            SessionBusRealm::Surface(13),
            Some("unix:path=/tmp/manifold-unused-host-bus".to_string()),
        )
        .unwrap();
        let connection = ConnectionBuilder::address(bus.address())
            .unwrap()
            .build()
            .unwrap();
        let proxy = DBusProxy::new(&connection).unwrap();
        proxy.hello().unwrap();

        let notifications = BusName::try_from(NOTIFICATIONS_DESTINATION).unwrap();
        assert!(proxy.name_has_owner(notifications.clone()).unwrap());
        assert_eq!(
            proxy.get_name_owner(notifications).unwrap().as_str(),
            NOTIFICATIONS_UNIQUE_NAME
        );
    }

    fn primary_header(endian: u8, body_len: u32, fields_len: u32) -> [u8; 16] {
        let mut primary = [0; DBUS_PRIMARY_HEADER_LEN];
        *primary.first_mut().expect("primary header endian byte") = endian;
        let (body, fields) = if endian == b'B' {
            (body_len.to_be_bytes(), fields_len.to_be_bytes())
        } else {
            (body_len.to_le_bytes(), fields_len.to_le_bytes())
        };
        primary
            .get_mut(4..8)
            .expect("primary body length")
            .copy_from_slice(&body);
        primary
            .get_mut(12..16)
            .expect("primary fields length")
            .copy_from_slice(&fields);
        primary
    }

    #[test]
    fn wire_header_checks_endian_alignment_and_exact_length() {
        let little = DbusFrameHeader::decode(primary_header(b'l', 4, 1)).unwrap();
        assert_eq!(little.remaining_len().unwrap(), 12);
        assert_eq!(little.total_len().unwrap(), 28);

        let big = DbusFrameHeader::decode(primary_header(b'B', 9, 8)).unwrap();
        assert_eq!(big.remaining_len().unwrap(), 17);
        assert_eq!(big.total_len().unwrap(), 33);
        assert!(DbusFrameHeader::decode(primary_header(b'x', 0, 0)).is_err());
    }

    #[test]
    fn wire_reader_rejects_short_and_oversized_frames_before_allocation() {
        assert!(read_message_bytes(&mut Cursor::new([b'l'; 8])).is_err());
        let oversized = primary_header(
            b'l',
            u32::try_from(MAX_DBUS_FRAME_LEN).expect("frame ceiling fits u32"),
            0,
        );
        assert!(read_message_bytes(&mut Cursor::new(oversized)).is_err());
        let overflowing = primary_header(b'l', u32::MAX, u32::MAX);
        assert!(read_message_bytes(&mut Cursor::new(overflowing)).is_err());
    }

    #[test]
    fn zbus_boundary_rejects_short_and_length_mismatched_storage() {
        assert!(parse_message(Vec::new()).is_err());
        assert!(parse_message(vec![b'l'; DBUS_PRIMARY_HEADER_LEN - 1]).is_err());
        assert!(parse_message(primary_header(b'l', 1, 0).to_vec()).is_err());
    }
}
