use std::{
    collections::HashMap,
    env,
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsFd, OwnedFd},
    sync::{Arc, Mutex},
    thread,
};

use anyhow::{Context, Result, anyhow};
use calloop::{EventLoop, LoopSignal, channel};
use calloop_wayland_source::WaylandSource;
use smithay::{
    input::Seat,
    reexports::rustix,
    wayland::selection::{
        SelectionSource, SelectionTarget,
        data_device::{
            clear_data_device_selection, request_data_device_client_selection,
            set_data_device_selection,
        },
    },
};
use tracing::{debug, info, warn};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, event_created_child,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use crate::state::Manifold;

const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;
const PREFERRED_TEXT_MIME_TYPES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

#[derive(Clone, Debug)]
pub struct ClipboardSelection {
    mime_types: Vec<String>,
    bytes: Arc<[u8]>,
}

impl ClipboardSelection {
    fn new(mime_type: String, offered_mime_types: Vec<String>, bytes: Vec<u8>) -> Self {
        let mut mime_types = Vec::new();
        push_unique(&mut mime_types, mime_type);
        for mime_type in offered_mime_types {
            push_unique(&mut mime_types, mime_type);
        }
        for mime_type in PREFERRED_TEXT_MIME_TYPES {
            push_unique(&mut mime_types, (*mime_type).to_string());
        }

        Self {
            mime_types,
            bytes: Arc::from(bytes),
        }
    }

    pub fn mime_types(&self) -> Vec<String> {
        self.mime_types.clone()
    }

    fn bytes(&self) -> Arc<[u8]> {
        Arc::clone(&self.bytes)
    }
}

#[derive(Debug)]
pub enum HostClipboardEvent {
    Selection(ClipboardSelection),
    Cleared,
}

#[derive(Clone, Debug)]
struct ClipboardSelectionSink {
    event_sender: channel::Sender<HostClipboardEvent>,
}

impl ClipboardSelectionSink {
    const fn new(event_sender: channel::Sender<HostClipboardEvent>) -> Self {
        Self { event_sender }
    }

    fn send_selection(&self, selection: ClipboardSelection) {
        if self
            .event_sender
            .send(HostClipboardEvent::Selection(selection))
            .is_err()
        {
            debug!("manifold event loop no longer accepts host clipboard events");
        }
    }

    fn send_cleared(&self) {
        if self.event_sender.send(HostClipboardEvent::Cleared).is_err() {
            debug!("manifold event loop no longer accepts host clipboard events");
        }
    }
}

#[derive(Clone, Debug)]
pub struct HostClipboardBridge {
    inner: Arc<HostClipboardBridgeInner>,
}

#[derive(Debug)]
struct HostClipboardBridgeInner {
    command_sender: channel::Sender<HostClipboardCommand>,
}

impl HostClipboardBridge {
    pub fn start(event_sender: channel::Sender<HostClipboardEvent>) -> Option<Self> {
        if env::var_os("WAYLAND_DISPLAY").is_none() {
            debug!("host clipboard bridge disabled because WAYLAND_DISPLAY is not set");
            return None;
        }

        let (command_sender, command_receiver) = channel::channel();
        let spawn_result = thread::Builder::new()
            .name("manifold-host-clipboard".to_string())
            .spawn(move || {
                if let Err(err) = run_clipboard_data_control(
                    ClipboardSelectionSink::new(event_sender),
                    command_receiver,
                ) {
                    warn!(?err, "host clipboard bridge stopped");
                }
            });

        match spawn_result {
            Ok(_handle) => Some(Self {
                inner: Arc::new(HostClipboardBridgeInner { command_sender }),
            }),
            Err(err) => {
                warn!(?err, "failed to spawn host clipboard bridge");
                None
            }
        }
    }

    pub fn set_selection(&self, selection: ClipboardSelection) {
        debug!(
            bytes = selection.bytes.len(),
            "queue host clipboard selection update"
        );
        if self
            .inner
            .command_sender
            .send(HostClipboardCommand::SetSelection(selection))
            .is_err()
        {
            debug!("host clipboard bridge is not accepting selection updates");
        }
    }

    pub fn clear(&self) {
        if self
            .inner
            .command_sender
            .send(HostClipboardCommand::Clear)
            .is_err()
        {
            debug!("host clipboard bridge is not accepting clear requests");
        }
    }
}

impl Drop for HostClipboardBridgeInner {
    fn drop(&mut self) {
        if self
            .command_sender
            .send(HostClipboardCommand::Shutdown)
            .is_err()
        {
            debug!("host clipboard bridge already stopped");
        }
    }
}

#[derive(Debug)]
enum HostClipboardCommand {
    SetSelection(ClipboardSelection),
    Clear,
    Shutdown,
}

#[derive(Debug)]
enum HostClipboardProtocol {
    Ext {
        _manager: ExtDataControlManagerV1,
        device: ExtDataControlDeviceV1,
    },
    Wlr {
        _manager: ZwlrDataControlManagerV1,
        device: ZwlrDataControlDeviceV1,
    },
}

#[derive(Debug)]
enum HostActiveSource {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

impl HostActiveSource {
    fn id(&self) -> wayland_client::backend::ObjectId {
        match self {
            Self::Ext(source) => source.id(),
            Self::Wlr(source) => source.id(),
        }
    }
}

#[derive(Clone, Debug)]
struct HostSourceData {
    selection: ClipboardSelection,
}

#[derive(Debug)]
struct HostOfferData {
    mime_types: Vec<String>,
}

#[derive(Debug)]
struct HostClipboardState {
    connection: Connection,
    queue_handle: QueueHandle<Self>,
    protocol: HostClipboardProtocol,
    selection_sink: ClipboardSelectionSink,
    offers: HashMap<wayland_client::backend::ObjectId, HostOfferData>,
    active_source: Option<HostActiveSource>,
    last_published_bytes: Arc<Mutex<Option<Vec<u8>>>>,
    loop_signal: LoopSignal,
}

impl HostClipboardState {
    fn set_host_selection(&mut self, selection: &ClipboardSelection) {
        debug!(
            bytes = selection.bytes.len(),
            "set host clipboard selection"
        );
        match self.last_published_bytes.lock() {
            Ok(mut last_published_bytes) => {
                *last_published_bytes = Some(selection.bytes.as_ref().to_vec());
            }
            Err(err) => warn!(?err, "host clipboard loop guard is poisoned"),
        }
        let active_source = match &self.protocol {
            HostClipboardProtocol::Ext {
                _manager: manager,
                device,
            } => {
                let source = create_ext_source(manager, &self.queue_handle, selection);
                device.set_selection(Some(&source));
                HostActiveSource::Ext(source)
            }
            HostClipboardProtocol::Wlr {
                _manager: manager,
                device,
            } => {
                let source = create_wlr_source(manager, &self.queue_handle, selection);
                device.set_selection(Some(&source));
                HostActiveSource::Wlr(source)
            }
        };
        self.active_source = Some(active_source);
        if let Err(err) = self.connection.flush() {
            warn!(?err, "failed to flush host clipboard selection update");
        }
    }

    fn clear_host_selection(&mut self) {
        match &self.protocol {
            HostClipboardProtocol::Ext { device, .. } => device.set_selection(None),
            HostClipboardProtocol::Wlr { device, .. } => device.set_selection(None),
        }
        self.active_source = None;
        match self.last_published_bytes.lock() {
            Ok(mut last_published_bytes) => {
                *last_published_bytes = None;
            }
            Err(err) => warn!(?err, "host clipboard loop guard is poisoned"),
        }
        if let Err(err) = self.connection.flush() {
            warn!(?err, "failed to flush host clipboard clear");
        }
    }

    fn add_offer(&mut self, offer: wayland_client::backend::ObjectId) {
        self.offers.insert(
            offer,
            HostOfferData {
                mime_types: Vec::new(),
            },
        );
    }

    fn add_offer_mime_type(&mut self, offer: wayland_client::backend::ObjectId, mime_type: String) {
        let entry = self.offers.entry(offer).or_insert_with(|| HostOfferData {
            mime_types: Vec::new(),
        });
        push_unique(&mut entry.mime_types, mime_type);
    }

    fn ext_selection(&self, offer: Option<ExtDataControlOfferV1>) {
        let Some(offer) = offer else {
            self.send_cleared();
            return;
        };
        let mime_types = self
            .offers
            .get(&offer.id())
            .map(|offer| offer.mime_types.clone())
            .unwrap_or_default();
        self.request_ext_offer(&offer, mime_types);
    }

    fn wlr_selection(&self, offer: Option<ZwlrDataControlOfferV1>) {
        let Some(offer) = offer else {
            self.send_cleared();
            return;
        };
        let mime_types = self
            .offers
            .get(&offer.id())
            .map(|offer| offer.mime_types.clone())
            .unwrap_or_default();
        self.request_wlr_offer(&offer, mime_types);
    }

    fn request_ext_offer(&self, offer: &ExtDataControlOfferV1, mime_types: Vec<String>) {
        let Some(mime_type) = choose_text_mime_type(&mime_types) else {
            debug!(
                ?mime_types,
                "host clipboard offer has no supported text MIME type"
            );
            return;
        };
        let Ok((read_fd, write_fd)) = rustix::pipe::pipe() else {
            warn!("failed to create pipe for host clipboard offer");
            return;
        };
        offer.receive(mime_type.clone(), write_fd.as_fd());
        drop(write_fd);
        if let Err(err) = self.connection.flush() {
            warn!(?err, "failed to flush host clipboard receive request");
            return;
        }
        self.spawn_host_offer_reader(mime_type, mime_types, read_fd);
    }

    fn request_wlr_offer(&self, offer: &ZwlrDataControlOfferV1, mime_types: Vec<String>) {
        let Some(mime_type) = choose_text_mime_type(&mime_types) else {
            debug!(
                ?mime_types,
                "host clipboard offer has no supported text MIME type"
            );
            return;
        };
        let Ok((read_fd, write_fd)) = rustix::pipe::pipe() else {
            warn!("failed to create pipe for host clipboard offer");
            return;
        };
        offer.receive(mime_type.clone(), write_fd.as_fd());
        drop(write_fd);
        if let Err(err) = self.connection.flush() {
            warn!(?err, "failed to flush host clipboard receive request");
            return;
        }
        self.spawn_host_offer_reader(mime_type, mime_types, read_fd);
    }

    fn spawn_host_offer_reader(
        &self,
        mime_type: String,
        mime_types: Vec<String>,
        read_fd: OwnedFd,
    ) {
        let selection_sink = self.selection_sink.clone();
        let last_published_bytes = Arc::clone(&self.last_published_bytes);
        let spawn_result = thread::Builder::new()
            .name("manifold-host-clipboard-read".to_string())
            .spawn(move || match read_fd_limited(read_fd) {
                Ok(bytes) => {
                    if matches_last_published_selection(&last_published_bytes, &bytes) {
                        debug!(
                            bytes = bytes.len(),
                            "ignore echoed host clipboard selection"
                        );
                        return;
                    }
                    let selection = ClipboardSelection::new(mime_type, mime_types, bytes);
                    selection_sink.send_selection(selection);
                }
                Err(err) => warn!(?err, "failed to read host clipboard selection"),
            });
        if let Err(err) = spawn_result {
            warn!(?err, "failed to spawn host clipboard reader");
        }
    }

    fn source_cancelled(&mut self, id: &wayland_client::backend::ObjectId) {
        if self
            .active_source
            .as_ref()
            .is_some_and(|source| source.id() == *id)
        {
            self.active_source = None;
        }
    }

    fn send_cleared(&self) {
        self.selection_sink.send_cleared();
    }
}

fn run_clipboard_data_control(
    selection_sink: ClipboardSelectionSink,
    command_receiver: channel::Channel<HostClipboardCommand>,
) -> Result<()> {
    let connection = Connection::connect_to_env().context("connect to host Wayland compositor")?;
    let (globals, event_queue) = registry_queue_init::<HostClipboardState>(&connection)
        .context("read host Wayland globals")?;
    let queue_handle = event_queue.handle();
    let seat = globals
        .bind::<wl_seat::WlSeat, _, _>(&queue_handle, 1..=9, ())
        .context("bind host wl_seat")?;
    let protocol = bind_host_clipboard_protocol(&globals, &queue_handle, &seat)?;

    let mut event_loop =
        EventLoop::<HostClipboardState>::try_new().context("create host clipboard event loop")?;
    let loop_signal = event_loop.get_signal();
    WaylandSource::new(connection.clone(), event_queue)
        .insert(event_loop.handle())
        .map_err(|err| anyhow!("insert host Wayland clipboard source: {err}"))?;
    event_loop
        .handle()
        .insert_source(command_receiver, |event, (), state| match event {
            channel::Event::Msg(HostClipboardCommand::SetSelection(selection)) => {
                state.set_host_selection(&selection);
            }
            channel::Event::Msg(HostClipboardCommand::Clear) => {
                state.clear_host_selection();
            }
            channel::Event::Msg(HostClipboardCommand::Shutdown) | channel::Event::Closed => {
                state.loop_signal.stop();
            }
        })
        .map_err(|err| anyhow!("insert host clipboard command source: {err}"))?;

    let mut state = HostClipboardState {
        connection,
        queue_handle,
        protocol,
        selection_sink,
        offers: HashMap::new(),
        active_source: None,
        last_published_bytes: Arc::new(Mutex::new(None)),
        loop_signal,
    };

    info!("clipboard data-control bridge started");
    event_loop
        .run(None, &mut state, |_| {})
        .context("run clipboard data-control event loop")
}

fn bind_host_clipboard_protocol(
    globals: &wayland_client::globals::GlobalList,
    queue_handle: &QueueHandle<HostClipboardState>,
    seat: &wl_seat::WlSeat,
) -> Result<HostClipboardProtocol> {
    if let Ok(manager) = globals.bind::<ExtDataControlManagerV1, _, _>(queue_handle, 1..=1, ()) {
        let device = manager.get_data_device(seat, queue_handle, ());
        return Ok(HostClipboardProtocol::Ext {
            _manager: manager,
            device,
        });
    }

    if let Ok(manager) = globals.bind::<ZwlrDataControlManagerV1, _, _>(queue_handle, 1..=2, ()) {
        let device = manager.get_data_device(seat, queue_handle, ());
        return Ok(HostClipboardProtocol::Wlr {
            _manager: manager,
            device,
        });
    }

    Err(anyhow!(
        "host Wayland compositor does not expose ext-data-control-v1 or zwlr-data-control-v1"
    ))
}

fn create_ext_source(
    manager: &ExtDataControlManagerV1,
    queue_handle: &QueueHandle<HostClipboardState>,
    selection: &ClipboardSelection,
) -> ExtDataControlSourceV1 {
    let source = manager.create_data_source(
        queue_handle,
        HostSourceData {
            selection: selection.clone(),
        },
    );
    for mime_type in selection.mime_types() {
        source.offer(mime_type);
    }
    source
}

fn create_wlr_source(
    manager: &ZwlrDataControlManagerV1,
    queue_handle: &QueueHandle<HostClipboardState>,
    selection: &ClipboardSelection,
) -> ZwlrDataControlSourceV1 {
    let source = manager.create_data_source(
        queue_handle,
        HostSourceData {
            selection: selection.clone(),
        },
    );
    for mime_type in selection.mime_types() {
        source.offer(mime_type);
    }
    source
}

impl Manifold {
    pub(crate) fn handle_host_clipboard_event(&self, event: HostClipboardEvent) {
        match event {
            HostClipboardEvent::Selection(selection) => {
                set_data_device_selection::<Self>(
                    &self.display_handle,
                    &self.seat,
                    selection.mime_types(),
                    selection,
                );
            }
            HostClipboardEvent::Cleared => {
                clear_data_device_selection::<Self>(&self.display_handle, &self.seat);
            }
        }
    }

    pub(crate) fn sync_internal_clipboard_to_host(
        &self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        seat: Seat<Self>,
    ) {
        debug!(
            ?ty,
            has_source = source.is_some(),
            "internal selection changed"
        );
        if ty != SelectionTarget::Clipboard {
            return;
        }

        let Some(bridge) = self.host_clipboard_bridge.clone() else {
            return;
        };

        let Some(source) = source else {
            bridge.clear();
            return;
        };

        let mime_types = source.mime_types();
        let Some(mime_type) = choose_text_mime_type(&mime_types) else {
            debug!(
                ?mime_types,
                "internal clipboard has no supported text MIME type"
            );
            return;
        };

        debug!(?mime_type, "schedule internal clipboard read for host sync");
        self.loop_handle.insert_idle(move |_state| {
            let Ok((read_fd, write_fd)) = rustix::pipe::pipe() else {
                warn!("failed to create pipe for internal clipboard selection");
                return;
            };
            if let Err(err) =
                request_data_device_client_selection::<Self>(&seat, mime_type.clone(), write_fd)
            {
                warn!(?err, "failed to request internal clipboard selection");
                return;
            }
            debug!("requested internal clipboard selection");
            let spawn_result = thread::Builder::new()
                .name("manifold-internal-clipboard-read".to_string())
                .spawn(move || match read_fd_limited(read_fd) {
                    Ok(bytes) => {
                        debug!(bytes = bytes.len(), "read internal clipboard selection");
                        bridge.set_selection(ClipboardSelection::new(mime_type, mime_types, bytes));
                    }
                    Err(err) => warn!(?err, "failed to read internal clipboard selection"),
                });
            if let Err(err) = spawn_result {
                warn!(?err, "failed to spawn internal clipboard reader");
            }
        });
    }
}

pub fn send_clipboard_selection(selection: &ClipboardSelection, mime_type: String, fd: OwnedFd) {
    let bytes = selection.bytes();
    let spawn_result = thread::Builder::new()
        .name("manifold-clipboard-write".to_string())
        .spawn(move || {
            if let Err(err) = write_fd(fd, bytes.as_ref()) {
                warn!(?err, ?mime_type, "failed to send clipboard selection");
            }
        });
    if let Err(err) = spawn_result {
        warn!(?err, "failed to spawn clipboard writer");
    }
}

fn choose_text_mime_type(mime_types: &[String]) -> Option<String> {
    for preferred in PREFERRED_TEXT_MIME_TYPES {
        if mime_types
            .iter()
            .any(|mime_type| mime_type.eq_ignore_ascii_case(preferred))
        {
            return Some((*preferred).to_string());
        }
    }

    mime_types
        .iter()
        .find(|mime_type| mime_type.to_ascii_lowercase().starts_with("text/"))
        .cloned()
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(&value))
    {
        values.push(value);
    }
}

fn matches_last_published_selection(
    last_published_bytes: &Mutex<Option<Vec<u8>>>,
    bytes: &[u8],
) -> bool {
    match last_published_bytes.lock() {
        Ok(last_published_bytes) => last_published_bytes
            .as_ref()
            .is_some_and(|last_published_bytes| last_published_bytes == bytes),
        Err(err) => {
            warn!(?err, "host clipboard loop guard is poisoned");
            false
        }
    }
}

fn read_fd_limited(fd: OwnedFd) -> io::Result<Vec<u8>> {
    let file = File::from(fd);
    let mut reader = file.take((MAX_CLIPBOARD_BYTES + 1) as u64);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CLIPBOARD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "clipboard payload exceeds manifold limit",
        ));
    }
    Ok(bytes)
}

fn write_fd(fd: OwnedFd, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::from(fd);
    file.write_all(bytes)
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for HostClipboardState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for HostClipboardState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for HostClipboardState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtDataControlManagerV1,
        _event: <ExtDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for HostClipboardState {
    event_created_child!(HostClipboardState, ExtDataControlDeviceV1, [
        0 => (ExtDataControlOfferV1, ())
    ]);

    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => state.add_offer(id.id()),
            ext_data_control_device_v1::Event::Selection { id } => state.ext_selection(id),
            ext_data_control_device_v1::Event::Finished => state.loop_signal.stop(),
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlOfferV1, ()> for HostClipboardState {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.add_offer_mime_type(proxy.id(), mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, HostSourceData> for HostClipboardState {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        data: &HostSourceData,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                send_clipboard_selection(&data.selection, mime_type, fd);
            }
            ext_data_control_source_v1::Event::Cancelled => {
                state.source_cancelled(&proxy.id());
                proxy.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for HostClipboardState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for HostClipboardState {
    event_created_child!(HostClipboardState, ZwlrDataControlDeviceV1, [
        0 => (ZwlrDataControlOfferV1, ())
    ]);

    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => state.add_offer(id.id()),
            zwlr_data_control_device_v1::Event::Selection { id } => state.wlr_selection(id),
            zwlr_data_control_device_v1::Event::Finished => state.loop_signal.stop(),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for HostClipboardState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.add_offer_mime_type(proxy.id(), mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, HostSourceData> for HostClipboardState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        data: &HostSourceData,
        _conn: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                send_clipboard_selection(&data.selection, mime_type, fd);
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                state.source_cancelled(&proxy.id());
                proxy.destroy();
            }
            _ => {}
        }
    }
}
