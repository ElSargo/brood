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

const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;
const TEXT_MIME_TYPES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];
const IMAGE_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardContent {
    Text(String),
    Image { mime_type: String, bytes: Vec<u8> },
}

#[derive(Clone, Debug)]
pub struct ClipboardBridge {
    inner: Arc<ClipboardBridgeInner>,
}

#[derive(Debug)]
struct ClipboardBridgeInner {
    command_sender: channel::Sender<ClipboardCommand>,
    latest_content: Arc<Mutex<Option<ClipboardContent>>>,
}

impl ClipboardBridge {
    pub fn start() -> Option<Self> {
        env::var_os("WAYLAND_DISPLAY")?;
        let latest_content = Arc::new(Mutex::new(None));
        let thread_latest_content = Arc::clone(&latest_content);
        let (command_sender, command_receiver) = channel::channel();
        let spawn = thread::Builder::new()
            .name("brood-client-clipboard".to_string())
            .spawn(move || {
                if let Err(error) =
                    run_clipboard_data_control(thread_latest_content, command_receiver)
                {
                    eprintln!("presentation clipboard bridge stopped: {error:#}");
                }
            });
        match spawn {
            Ok(_handle) => Some(Self {
                inner: Arc::new(ClipboardBridgeInner {
                    command_sender,
                    latest_content,
                }),
            }),
            Err(error) => {
                eprintln!("failed to start presentation clipboard bridge: {error}");
                None
            }
        }
    }

    pub fn set_text(&self, text: String) {
        if let Ok(mut latest) = self.inner.latest_content.lock() {
            *latest = Some(ClipboardContent::Text(text.clone()));
        }
        if self
            .inner
            .command_sender
            .send(ClipboardCommand::SetText(text))
            .is_err()
        {
            eprintln!("presentation clipboard bridge is not accepting copied text");
        }
    }

    pub fn content(&self) -> Option<ClipboardContent> {
        self.inner.latest_content.lock().ok()?.clone()
    }
}

impl Drop for ClipboardBridgeInner {
    fn drop(&mut self) {
        let _ignored = self.command_sender.send(ClipboardCommand::Shutdown);
    }
}

#[derive(Debug)]
enum ClipboardCommand {
    SetText(String),
    Shutdown,
}

#[derive(Debug)]
enum ClipboardProtocol {
    Ext {
        manager: ExtDataControlManagerV1,
        device: ExtDataControlDeviceV1,
    },
    Wlr {
        manager: ZwlrDataControlManagerV1,
        device: ZwlrDataControlDeviceV1,
    },
}

#[derive(Debug)]
enum ActiveSource {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

impl ActiveSource {
    fn id(&self) -> wayland_client::backend::ObjectId {
        match self {
            Self::Ext(source) => source.id(),
            Self::Wlr(source) => source.id(),
        }
    }
}

#[derive(Clone, Debug)]
struct SourceData {
    bytes: Arc<[u8]>,
}

#[derive(Debug, Default)]
struct OfferData {
    mime_types: Vec<String>,
}

#[derive(Debug)]
struct ClipboardState {
    connection: Connection,
    queue_handle: QueueHandle<Self>,
    protocol: ClipboardProtocol,
    offers: HashMap<wayland_client::backend::ObjectId, OfferData>,
    active_source: Option<ActiveSource>,
    latest_content: Arc<Mutex<Option<ClipboardContent>>>,
    loop_signal: LoopSignal,
}

impl ClipboardState {
    fn set_text(&mut self, text: String) {
        let data = SourceData {
            bytes: Arc::from(text.into_bytes()),
        };
        self.active_source = Some(match &self.protocol {
            ClipboardProtocol::Ext { manager, device } => {
                let source = manager.create_data_source(&self.queue_handle, data);
                for mime_type in TEXT_MIME_TYPES {
                    source.offer((*mime_type).to_string());
                }
                device.set_selection(Some(&source));
                ActiveSource::Ext(source)
            }
            ClipboardProtocol::Wlr { manager, device } => {
                let source = manager.create_data_source(&self.queue_handle, data);
                for mime_type in TEXT_MIME_TYPES {
                    source.offer((*mime_type).to_string());
                }
                device.set_selection(Some(&source));
                ActiveSource::Wlr(source)
            }
        });
        if let Err(error) = self.connection.flush() {
            eprintln!("failed to publish presentation clipboard text: {error}");
        }
    }

    fn add_offer(&mut self, id: wayland_client::backend::ObjectId) {
        self.offers.entry(id).or_default();
    }

    fn add_mime_type(&mut self, id: wayland_client::backend::ObjectId, mime_type: String) {
        let offer = self.offers.entry(id).or_default();
        if !offer.mime_types.contains(&mime_type) {
            offer.mime_types.push(mime_type);
        }
    }

    fn ext_selection(&self, offer: Option<ExtDataControlOfferV1>) {
        let Some(offer) = offer else {
            self.clear_latest();
            return;
        };
        let mime_types = self.offer_mime_types(&offer.id());
        let Some((mime_type, kind)) = choose_content_mime_type(&mime_types) else {
            return;
        };
        self.clear_latest();
        let Ok((read_fd, write_fd)) = rustix::pipe::pipe() else {
            return;
        };
        offer.receive(mime_type.clone(), write_fd.as_fd());
        drop(write_fd);
        if self.connection.flush().is_ok() {
            spawn_offer_reader(Arc::clone(&self.latest_content), read_fd, mime_type, kind);
        }
    }

    fn wlr_selection(&self, offer: Option<ZwlrDataControlOfferV1>) {
        let Some(offer) = offer else {
            self.clear_latest();
            return;
        };
        let mime_types = self.offer_mime_types(&offer.id());
        let Some((mime_type, kind)) = choose_content_mime_type(&mime_types) else {
            return;
        };
        self.clear_latest();
        let Ok((read_fd, write_fd)) = rustix::pipe::pipe() else {
            return;
        };
        offer.receive(mime_type.clone(), write_fd.as_fd());
        drop(write_fd);
        if self.connection.flush().is_ok() {
            spawn_offer_reader(Arc::clone(&self.latest_content), read_fd, mime_type, kind);
        }
    }

    fn offer_mime_types(&self, id: &wayland_client::backend::ObjectId) -> Vec<String> {
        self.offers
            .get(id)
            .map(|offer| offer.mime_types.clone())
            .unwrap_or_default()
    }

    fn clear_latest(&self) {
        if let Ok(mut latest) = self.latest_content.lock() {
            *latest = None;
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
}

fn run_clipboard_data_control(
    latest_content: Arc<Mutex<Option<ClipboardContent>>>,
    command_receiver: channel::Channel<ClipboardCommand>,
) -> Result<()> {
    let connection = Connection::connect_to_env().context("connect to Wayland compositor")?;
    let (globals, event_queue) =
        registry_queue_init::<ClipboardState>(&connection).context("read Wayland globals")?;
    let queue_handle = event_queue.handle();
    let seat = globals
        .bind::<wl_seat::WlSeat, _, _>(&queue_handle, 1..=9, ())
        .context("bind clipboard seat")?;
    let protocol = bind_protocol(&globals, &queue_handle, &seat)?;
    let mut event_loop =
        EventLoop::<ClipboardState>::try_new().context("create clipboard event loop")?;
    let loop_signal = event_loop.get_signal();
    WaylandSource::new(connection.clone(), event_queue)
        .insert(event_loop.handle())
        .map_err(|error| anyhow!("insert clipboard Wayland source: {error}"))?;
    event_loop
        .handle()
        .insert_source(command_receiver, |event, (), state| match event {
            channel::Event::Msg(ClipboardCommand::SetText(text)) => state.set_text(text),
            channel::Event::Msg(ClipboardCommand::Shutdown) | channel::Event::Closed => {
                state.loop_signal.stop();
            }
        })
        .map_err(|error| anyhow!("insert clipboard command source: {error}"))?;
    let mut state = ClipboardState {
        connection,
        queue_handle,
        protocol,
        offers: HashMap::new(),
        active_source: None,
        latest_content,
        loop_signal,
    };
    event_loop
        .run(None, &mut state, |_| {})
        .context("run clipboard event loop")
}

fn bind_protocol(
    globals: &wayland_client::globals::GlobalList,
    queue_handle: &QueueHandle<ClipboardState>,
    seat: &wl_seat::WlSeat,
) -> Result<ClipboardProtocol> {
    if let Ok(manager) = globals.bind::<ExtDataControlManagerV1, _, _>(queue_handle, 1..=1, ()) {
        let device = manager.get_data_device(seat, queue_handle, ());
        return Ok(ClipboardProtocol::Ext { manager, device });
    }
    if let Ok(manager) = globals.bind::<ZwlrDataControlManagerV1, _, _>(queue_handle, 1..=2, ()) {
        let device = manager.get_data_device(seat, queue_handle, ());
        return Ok(ClipboardProtocol::Wlr { manager, device });
    }
    Err(anyhow!(
        "Wayland compositor does not expose ext-data-control-v1 or zwlr-data-control-v1"
    ))
}

fn choose_text_mime_type(mime_types: &[String]) -> Option<String> {
    TEXT_MIME_TYPES
        .iter()
        .find(|preferred| {
            mime_types
                .iter()
                .any(|mime_type| mime_type.eq_ignore_ascii_case(preferred))
        })
        .map(|mime_type| (*mime_type).to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClipboardContentKind {
    Text,
    Image,
}

fn choose_content_mime_type(mime_types: &[String]) -> Option<(String, ClipboardContentKind)> {
    IMAGE_MIME_TYPES
        .iter()
        .find(|preferred| {
            mime_types
                .iter()
                .any(|mime_type| mime_type.eq_ignore_ascii_case(preferred))
        })
        .map(|mime_type| ((*mime_type).to_string(), ClipboardContentKind::Image))
        .or_else(|| {
            choose_text_mime_type(mime_types)
                .map(|mime_type| (mime_type, ClipboardContentKind::Text))
        })
}

fn spawn_offer_reader(
    latest_content: Arc<Mutex<Option<ClipboardContent>>>,
    fd: OwnedFd,
    mime_type: String,
    kind: ClipboardContentKind,
) {
    let spawn = thread::Builder::new()
        .name("brood-client-clipboard-read".to_string())
        .spawn(move || match read_fd_limited(fd) {
            Ok(bytes) => {
                let content = match kind {
                    ClipboardContentKind::Text => match String::from_utf8(bytes) {
                        Ok(text) => ClipboardContent::Text(text),
                        Err(error) => {
                            eprintln!("clipboard text is not UTF-8: {error}");
                            return;
                        }
                    },
                    ClipboardContentKind::Image => ClipboardContent::Image { mime_type, bytes },
                };
                if let Ok(mut latest) = latest_content.lock() {
                    *latest = Some(content);
                }
            }
            Err(error) => eprintln!("failed to read clipboard content: {error}"),
        });
    if let Err(error) = spawn {
        eprintln!("failed to start clipboard reader: {error}");
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
            "clipboard content exceeds the presentation limit",
        ));
    }
    Ok(bytes)
}

fn send_source_bytes(data: &SourceData, mime_type: &str, fd: OwnedFd) {
    let bytes = Arc::clone(&data.bytes);
    let mime_type = mime_type.to_string();
    let spawn = thread::Builder::new()
        .name("brood-client-clipboard-write".to_string())
        .spawn(move || {
            let mut file = File::from(fd);
            if let Err(error) = file.write_all(bytes.as_ref()) {
                eprintln!("failed to send clipboard MIME type {mime_type}: {error}");
            }
        });
    if let Err(error) = spawn {
        eprintln!("failed to start clipboard writer: {error}");
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ClipboardState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for ClipboardState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for ClipboardState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtDataControlManagerV1,
        _event: <ExtDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for ClipboardState {
    event_created_child!(ClipboardState, ExtDataControlDeviceV1, [
        0 => (ExtDataControlOfferV1, ())
    ]);

    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _data: &(),
        _connection: &Connection,
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

impl Dispatch<ExtDataControlOfferV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.add_mime_type(proxy.id(), mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, SourceData> for ClipboardState {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        data: &SourceData,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                send_source_bytes(data, &mime_type, fd);
            }
            ext_data_control_source_v1::Event::Cancelled => {
                state.source_cancelled(&proxy.id());
                proxy.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for ClipboardState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for ClipboardState {
    event_created_child!(ClipboardState, ZwlrDataControlDeviceV1, [
        0 => (ZwlrDataControlOfferV1, ())
    ]);

    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &(),
        _connection: &Connection,
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

impl Dispatch<ZwlrDataControlOfferV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.add_mime_type(proxy.id(), mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, SourceData> for ClipboardState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        data: &SourceData,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                send_source_bytes(data, &mime_type, fd);
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                state.source_cancelled(&proxy.id());
                proxy.destroy();
            }
            _ => {}
        }
    }
}
