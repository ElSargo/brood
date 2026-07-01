#![allow(
    clippy::allow_attributes_without_reason,
    clippy::branches_sharing_code,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::let_underscore_must_use,
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::option_if_let_else,
    clippy::type_complexity,
    clippy::undocumented_unsafe_blocks,
    clippy::unreachable,
    reason = "tracked in issues.md: session bus forwarding still parses raw DBus messages and owns routing state inline"
)]
// TODO(lint-structure): replace the ad hoc DBus byte router with typed message/session abstractions.

use std::{
    collections::{BTreeMap, BTreeSet},
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

use anyhow::{Context, Result, anyhow};
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

const DBUS_DESTINATION: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";
const NOTIFICATIONS_DESTINATION: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_UNIQUE_NAME: &str = ":1.999999";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const NOTIFICATIONS_INTERFACE: &str = "org.freedesktop.Notifications";
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
    #[allow(dead_code)]
    Headless(u64),
}

pub struct SessionBusHandle {
    address: String,
    socket_path: PathBuf,
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SessionBusHandle {
    pub(crate) fn start(realm: SessionBusRealm, host_bus_address: Option<String>) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "manifold-inprocess-dbus-{}-{}",
            std::process::id(),
            realm.label()
        ));
        let _ = fs::remove_dir_all(&dir);
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
        let thread_stop = Arc::clone(&stop);
        let thread_state = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name(format!("manifold-dbus-{}", realm.label()))
            .spawn(move || accept_loop(listener, thread_state, thread_stop))
            .context("spawn in-process DBus realm thread")?;

        Ok(Self {
            address,
            socket_path,
            dir,
            stop,
            thread: Some(thread),
        })
    }

    pub(crate) fn address(&self) -> &str {
        &self.address
    }
}

impl Drop for SessionBusHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

struct RealmState {
    realm: SessionBusRealm,
    guid: String,
    host_bus_address: Option<String>,
    next_connection: u64,
    connections: BTreeMap<u64, ConnectionState>,
    names: BTreeMap<String, u64>,
}

impl RealmState {
    const fn new(realm: SessionBusRealm, guid: String, host_bus_address: Option<String>) -> Self {
        Self {
            realm,
            guid,
            host_bus_address,
            next_connection: 1,
            connections: BTreeMap::new(),
            names: BTreeMap::new(),
        }
    }

    fn insert_connection(&mut self, tx: mpsc::Sender<Vec<u8>>) -> u64 {
        let id = self.next_connection;
        self.next_connection += 1;
        self.connections.insert(
            id,
            ConnectionState {
                unique_name: None,
                owned_names: BTreeSet::new(),
                tx,
            },
        );
        id
    }

    fn unique_name(&mut self, connection_id: u64) -> String {
        let name = format!(":1.{connection_id}");
        if let Some(connection) = self.connections.get_mut(&connection_id)
            && connection.unique_name.is_none()
        {
            connection.unique_name = Some(name.clone());
            self.names.insert(name.clone(), connection_id);
        }
        name
    }

    fn name_owner(&self, name: &str) -> Option<u64> {
        if name == DBUS_DESTINATION {
            return Some(0);
        }
        if matches!(name, NOTIFICATIONS_DESTINATION | NOTIFICATIONS_UNIQUE_NAME)
            && self.host_bus_address.is_some()
        {
            return Some(0);
        }
        if let Some(stripped) = name.strip_prefix(':') {
            let _ = stripped;
            self.names.get(name).copied()
        } else {
            self.names.get(name).copied()
        }
    }

    fn remove_connection(&mut self, connection_id: u64) {
        if let Some(connection) = self.connections.remove(&connection_id) {
            if let Some(unique) = connection.unique_name {
                self.names.remove(&unique);
            }
            for name in connection.owned_names {
                self.names.remove(&name);
            }
        }
    }
}

struct ConnectionState {
    unique_name: Option<String>,
    owned_names: BTreeSet<String>,
    tx: mpsc::Sender<Vec<u8>>,
}

fn accept_loop(listener: UnixListener, state: Arc<Mutex<RealmState>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let connection_state = Arc::clone(&state);
                let _ = thread::Builder::new()
                    .name("manifold-dbus-client".to_string())
                    .spawn(move || {
                        if let Err(err) = handle_connection(stream, connection_state) {
                            debug!(?err, "in-process DBus client disconnected");
                        }
                    });
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

fn handle_connection(stream: UnixStream, state: Arc<Mutex<RealmState>>) -> Result<()> {
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
        .insert_connection(tx);

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

    let result = read_messages(&mut reader, connection_id, Arc::clone(&state));
    state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?
        .remove_connection(connection_id);
    drop(stream);
    let _ = writer_thread.join();
    result
}

fn authenticate(reader: &mut BufReader<UnixStream>, guid: &str) -> Result<()> {
    let mut initial = [0; 1];
    reader
        .get_mut()
        .read_exact(&mut initial)
        .context("read DBus auth nul")?;
    if initial[0] != 0 {
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
    connection_id: u64,
    state: Arc<Mutex<RealmState>>,
) -> Result<()> {
    loop {
        let bytes = read_message_bytes(reader)?;
        let message = parse_message(bytes.clone()).context("parse DBus message")?;
        route_message(connection_id, message, bytes, Arc::clone(&state))?;
    }
}

fn read_message_bytes(reader: &mut BufReader<UnixStream>) -> Result<Vec<u8>> {
    let mut primary = [0; 16];
    reader
        .read_exact(&mut primary)
        .context("read DBus primary header")?;
    let little = match primary[0] {
        b'l' => true,
        b'B' => false,
        other => return Err(anyhow!("unsupported DBus endian marker {other}")),
    };
    let body_len = read_u32(&primary[4..8], little) as usize;
    let fields_len = read_u32(&primary[12..16], little) as usize;
    let header_len = 16 + fields_len;
    let padding = padding_for_8_bytes(header_len);
    let rest_len = fields_len + padding + body_len;
    let mut bytes = primary.to_vec();
    bytes.resize(16 + rest_len, 0);
    reader
        .read_exact(&mut bytes[16..])
        .context("read DBus message body")?;
    Ok(bytes)
}

fn route_message(
    connection_id: u64,
    message: Message,
    original_bytes: Vec<u8>,
    state: Arc<Mutex<RealmState>>,
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
        return handle_dbus_method(connection_id, &message, member.as_deref(), state);
    }

    if header.message_type() == Type::MethodCall
        && is_notifications_destination(destination.as_deref())
        && path.as_deref() == Some(NOTIFICATIONS_PATH)
        && matches!(
            interface.as_deref(),
            Some(NOTIFICATIONS_INTERFACE | INTROSPECTABLE_INTERFACE)
        )
    {
        return handle_notifications_method(connection_id, &message, member.as_deref(), state);
    }

    let unique = ensure_unique(connection_id, &state)?;
    let outbound = match header.message_type() {
        Type::MethodCall => inject_sender(&message, &unique)?,
        Type::Signal => inject_signal_sender(&message, &unique)?,
        Type::MethodReturn | Type::Error => original_bytes,
    };

    let mut state = state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?;
    if let Some(destination) = destination
        && let Some(owner) = state.name_owner(&destination)
        && owner != connection_id
    {
        send_to(&mut state, owner, outbound);
        return Ok(());
    }

    if header.message_type() == Type::Signal {
        for (&id, connection) in &state.connections {
            if id != connection_id {
                let _ = connection.tx.send(outbound.clone());
            }
        }
    } else if header.message_type() == Type::MethodCall {
        send_error(
            &mut state,
            connection_id,
            &message,
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "destination has no owner",
        )?;
    }
    Ok(())
}

fn handle_dbus_method(
    connection_id: u64,
    message: &Message,
    member: Option<&str>,
    state: Arc<Mutex<RealmState>>,
) -> Result<()> {
    let mut state = state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?;
    match member {
        Some("Hello") => {
            let name = state.unique_name(connection_id);
            send_reply(&mut state, connection_id, message, &(name,))?;
        }
        Some("RequestName") => {
            let owner_unique = state.unique_name(connection_id);
            let (name, _flags): (String, u32) = message.body().deserialize()?;
            if valid_well_known_name(&name) {
                if let Some(existing) = state.names.get(&name).copied()
                    && existing != connection_id
                {
                    send_reply(&mut state, connection_id, message, &(3u32,))?;
                    return Ok(());
                }
                state.names.insert(name.clone(), connection_id);
                if let Some(connection) = state.connections.get_mut(&connection_id) {
                    connection.owned_names.insert(name.clone());
                }
                send_reply(&mut state, connection_id, message, &(1u32,))?;
                emit_name_owner_changed(&mut state, &name, "", &owner_unique)?;
            } else {
                send_error(
                    &mut state,
                    connection_id,
                    message,
                    "org.freedesktop.DBus.Error.InvalidArgs",
                    "invalid bus name",
                )?;
            }
        }
        Some("ReleaseName") => {
            let (name,): (String,) = message.body().deserialize()?;
            let old_owner = state
                .connections
                .get(&connection_id)
                .and_then(|connection| connection.unique_name.clone())
                .unwrap_or_default();
            let released = if state.names.get(&name) == Some(&connection_id) {
                state.names.remove(&name);
                if let Some(connection) = state.connections.get_mut(&connection_id) {
                    connection.owned_names.remove(&name);
                }
                1u32
            } else {
                2u32
            };
            send_reply(&mut state, connection_id, message, &(released,))?;
            if released == 1 {
                emit_name_owner_changed(&mut state, &name, &old_owner, "")?;
            }
        }
        Some("NameHasOwner") => {
            let (name,): (String,) = message.body().deserialize()?;
            let has_owner = state.name_owner(&name).is_some();
            send_reply(&mut state, connection_id, message, &(has_owner,))?;
        }
        Some("GetNameOwner") => {
            let (name,): (String,) = message.body().deserialize()?;
            if name == NOTIFICATIONS_DESTINATION && state.host_bus_address.is_some() {
                send_reply(
                    &mut state,
                    connection_id,
                    message,
                    &(NOTIFICATIONS_UNIQUE_NAME,),
                )?;
            } else if let Some(owner) = state.name_owner(&name) {
                let owner_name = state
                    .connections
                    .get(&owner)
                    .and_then(|connection| connection.unique_name.clone())
                    .unwrap_or_else(|| format!(":1.{owner}"));
                send_reply(&mut state, connection_id, message, &(owner_name,))?;
            } else {
                send_error(
                    &mut state,
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
                send_reply(&mut state, connection_id, message, &(2u32,))?;
            } else {
                send_error(
                    &mut state,
                    connection_id,
                    message,
                    "org.freedesktop.DBus.Error.ServiceUnknown",
                    "service activation is unavailable",
                )?;
            }
        }
        Some("ListNames") => {
            let mut names = vec![DBUS_DESTINATION.to_string()];
            if state.host_bus_address.is_some() {
                names.push(NOTIFICATIONS_DESTINATION.to_string());
                names.push(NOTIFICATIONS_UNIQUE_NAME.to_string());
            }
            names.extend(state.names.keys().cloned());
            send_reply(&mut state, connection_id, message, &(names,))?;
        }
        Some("AddMatch" | "RemoveMatch") => {
            send_reply(&mut state, connection_id, message, &())?;
        }
        Some("GetId") => {
            let guid = state.guid.clone();
            send_reply(&mut state, connection_id, message, &(guid,))?;
        }
        Some(other) => {
            warn!(
                ?other,
                realm = ?state.realm,
                "unsupported in-process DBus daemon method"
            );
            send_error(
                &mut state,
                connection_id,
                message,
                "org.freedesktop.DBus.Error.UnknownMethod",
                "unsupported bus method",
            )?;
        }
        None => {
            send_error(
                &mut state,
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

fn handle_notifications_method(
    connection_id: u64,
    message: &Message,
    member: Option<&str>,
    state: Arc<Mutex<RealmState>>,
) -> Result<()> {
    let host_bus_address = state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?
        .host_bus_address
        .clone();
    let Some(host_bus_address) = host_bus_address else {
        let mut state = state
            .lock()
            .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?;
        return send_error(
            &mut state,
            connection_id,
            message,
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "notification service is unavailable",
        );
    };

    let reply = match call_host_notifications_method(&host_bus_address, message, member) {
        Ok(reply) => reply,
        Err(err) => {
            warn!(
                ?err,
                ?member,
                "failed to proxy notification method to host session bus"
            );
            let mut state = state
                .lock()
                .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?;
            return send_error(
                &mut state,
                connection_id,
                message,
                "org.freedesktop.DBus.Error.Failed",
                &err.to_string(),
            );
        }
    };

    if let NotificationMethodReply::NotificationId(notification_id) = reply {
        spawn_notification_signal_forwarder(
            host_bus_address,
            Arc::clone(&state),
            connection_id,
            notification_id,
        );
        let mut state = state
            .lock()
            .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?;
        return send_reply_from(
            &mut state,
            connection_id,
            message,
            NOTIFICATIONS_UNIQUE_NAME,
            &(notification_id,),
        );
    }

    let mut state = state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?;
    match reply {
        NotificationMethodReply::Unit => send_reply_from(
            &mut state,
            connection_id,
            message,
            NOTIFICATIONS_UNIQUE_NAME,
            &(),
        ),
        NotificationMethodReply::Capabilities(capabilities) => send_reply_from(
            &mut state,
            connection_id,
            message,
            NOTIFICATIONS_UNIQUE_NAME,
            &(capabilities,),
        ),
        NotificationMethodReply::Introspection(xml) => send_reply_from(
            &mut state,
            connection_id,
            message,
            NOTIFICATIONS_UNIQUE_NAME,
            &(xml,),
        ),
        NotificationMethodReply::ServerInformation(info) => send_reply_from(
            &mut state,
            connection_id,
            message,
            NOTIFICATIONS_UNIQUE_NAME,
            &info,
        ),
        NotificationMethodReply::NotificationId(_) => unreachable!(),
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
            let (app_name, replaces_id, app_icon, summary, body, actions, hints, expire_timeout): (
                String,
                u32,
                String,
                String,
                String,
                Vec<String>,
                BTreeMap<String, OwnedValue>,
                i32,
            ) = message.body().deserialize()?;
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
    connection_id: u64,
    notification_id: u32,
) {
    let result = thread::Builder::new()
        .name(format!("manifold-notify-{notification_id}"))
        .spawn(move || {
            if let Err(err) = forward_notification_signals(
                &host_bus_address,
                state,
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
    state: Arc<Mutex<RealmState>>,
    connection_id: u64,
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
        match member.as_deref() {
            Some("ActionInvoked") => {
                let (id, action_key): (u32, String) = signal.body().deserialize()?;
                if id == notification_id {
                    send_notification_signal(
                        &state,
                        connection_id,
                        "ActionInvoked",
                        &(id, action_key),
                    )?;
                }
            }
            Some("ActivationToken") => {
                let (id, activation_token): (u32, String) = signal.body().deserialize()?;
                if id == notification_id {
                    send_notification_signal(
                        &state,
                        connection_id,
                        "ActivationToken",
                        &(id, activation_token),
                    )?;
                }
            }
            Some("NotificationClosed") => {
                let (id, reason): (u32, u32) = signal.body().deserialize()?;
                if id == notification_id {
                    send_notification_signal(
                        &state,
                        connection_id,
                        "NotificationClosed",
                        &(id, reason),
                    )?;
                    return Ok(());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn send_notification_signal<B>(
    state: &Arc<Mutex<RealmState>>,
    connection_id: u64,
    member: &str,
    body: &B,
) -> Result<()>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let signal = Message::signal(NOTIFICATIONS_PATH, NOTIFICATIONS_INTERFACE, member)?
        .sender(NOTIFICATIONS_UNIQUE_NAME)?
        .build(body)?;
    let mut state = state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?;
    send_to(&mut state, connection_id, signal.data().to_vec());
    Ok(())
}

fn emit_name_owner_changed(
    state: &mut RealmState,
    name: &str,
    old_owner: &str,
    new_owner: &str,
) -> Result<()> {
    let signal = Message::signal(DBUS_PATH, DBUS_INTERFACE, "NameOwnerChanged")?
        .sender(":1.0")?
        .build(&(name, old_owner, new_owner))?;
    broadcast(state, signal.data().to_vec(), None);
    Ok(())
}

fn send_reply<B>(
    state: &mut RealmState,
    connection_id: u64,
    message: &Message,
    body: &B,
) -> Result<()>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    send_reply_from(state, connection_id, message, ":1.0", body)
}

fn send_reply_from<B>(
    state: &mut RealmState,
    connection_id: u64,
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
    send_to(state, connection_id, reply.data().to_vec());
    Ok(())
}

fn send_error(
    state: &mut RealmState,
    connection_id: u64,
    message: &Message,
    name: &str,
    text: &str,
) -> Result<()> {
    let reply = Message::error(&message.header(), name)?
        .sender(":1.0")?
        .build(&(text,))?;
    send_to(state, connection_id, reply.data().to_vec());
    Ok(())
}

fn send_to(state: &mut RealmState, connection_id: u64, bytes: Vec<u8>) {
    if let Some(connection) = state.connections.get(&connection_id) {
        let _ = connection.tx.send(bytes);
    }
}

fn broadcast(state: &mut RealmState, bytes: Vec<u8>, except: Option<u64>) {
    for (&id, connection) in &state.connections {
        if Some(id) != except {
            let _ = connection.tx.send(bytes.clone());
        }
    }
}

fn ensure_unique(connection_id: u64, state: &Arc<Mutex<RealmState>>) -> Result<String> {
    Ok(state
        .lock()
        .map_err(|err| anyhow!("DBus realm state poisoned: {err}"))?
        .unique_name(connection_id))
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
    // The message body has already been parsed from a D-Bus frame. Rebuilding it with the
    // same signature only changes routing headers that a bus is expected to own.
    let rebuilt = unsafe { builder.build_raw_body(&body_bytes, signature, Vec::new())? };
    Ok(rebuilt.data().to_vec())
}

fn parse_message(bytes: Vec<u8>) -> Result<Message> {
    let endian = match bytes.first().copied() {
        Some(b'l') => Endian::Little,
        Some(b'B') => Endian::Big,
        Some(other) => return Err(anyhow!("unsupported DBus endian marker {other}")),
        None => return Err(anyhow!("empty DBus message")),
    };
    let data = Data::new(bytes, SerializedContext::new_dbus(endian, 0));
    // zbus validates headers while constructing Message; the unsafety is limited to accepting
    // bytes from a local client socket, as a D-Bus broker must do.
    Ok(unsafe { Message::from_bytes(data)? })
}

fn read_u32(bytes: &[u8], little: bool) -> u32 {
    let array = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if little {
        u32::from_le_bytes(array)
    } else {
        u32::from_be_bytes(array)
    }
}

const fn padding_for_8_bytes(len: usize) -> usize {
    (8 - (len % 8)) % 8
}

fn valid_well_known_name(name: &str) -> bool {
    !name.starts_with(':') && name.contains('.') && !name.is_empty()
}

fn make_guid(realm: SessionBusRealm) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let realm_id = match realm {
        SessionBusRealm::Surface(id) | SessionBusRealm::Headless(id) => id,
    };
    format!(
        "{:016x}{:08x}{:08x}",
        now as u64,
        std::process::id(),
        realm_id as u32
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
    clippy::unwrap_used,
    reason = "unit tests rely on inferred DBus bitflag defaults from zbus"
)]
mod tests {
    use super::{
        NOTIFICATIONS_DESTINATION, NOTIFICATIONS_UNIQUE_NAME, SessionBusHandle, SessionBusRealm,
    };
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
}
