use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
    os::fd::AsFd,
    path::PathBuf,
    sync::Arc,
    thread,
};

use anyhow::{Context, Result};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use rustix::pipe::{PipeFlags, pipe_with};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    backend::{Backend, ObjectId},
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::{DndAction, WlDataDeviceManager},
        wl_data_offer::{self, WlDataOffer},
        wl_registry::WlRegistry,
        wl_seat::{self, WlSeat},
        wl_surface::WlSurface,
    },
};
use winit::window::Window;

const URI_LIST_MIME: &str = "text/uri-list";
const MAX_URI_LIST_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DropEvent {
    pub path: PathBuf,
    pub position: (i32, i32),
}

pub struct DropBridge {
    _device: WlDataDevice,
    _seat: WlSeat,
    _manager: WlDataDeviceManager,
    state: DropState,
    queue: EventQueue<DropState>,
    connection: Connection,
}

struct DropState {
    surface: ObjectId,
    offers: HashMap<u32, OfferState>,
    active_offer: Option<u32>,
    position: (i32, i32),
    deliver: Arc<dyn Fn(DropEvent) + Send + Sync>,
}

struct OfferState {
    proxy: WlDataOffer,
    mime_types: HashSet<String>,
}

impl DropBridge {
    pub fn new(
        window: &Window,
        deliver: impl Fn(DropEvent) + Send + Sync + 'static,
    ) -> Result<Option<Self>> {
        let display = match window.display_handle()?.as_raw() {
            RawDisplayHandle::Wayland(display) => display.display,
            _ => return Ok(None),
        };
        let surface = match window.window_handle()?.as_raw() {
            RawWindowHandle::Wayland(window) => window.surface,
            _ => return Ok(None),
        };

        // SAFETY: Winit guarantees that both raw handles remain valid for the
        // lifetime of `window`. `DropBridge` is owned beside that window and its
        // guest backend never closes the Winit-owned display.
        let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        // SAFETY: The raw window handle is a live wl_surface on the same display,
        // and the stored ID is discarded before the Winit window is destroyed.
        let surface =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.as_ptr().cast()) }
                .context("adopt the Winit Wayland surface for drag and drop")?;
        let (globals, mut queue) =
            registry_queue_init::<DropState>(&connection).context("read Wayland globals")?;
        let qh = queue.handle();
        let manager = globals
            .bind::<WlDataDeviceManager, _, _>(&qh, 1..=3, ())
            .context("bind wl_data_device_manager")?;
        let seat = globals
            .bind::<WlSeat, _, _>(&qh, 1..=9, ())
            .context("bind a Wayland seat for drag and drop")?;
        let device = manager.get_data_device(&seat, &qh, ());
        let mut state = DropState {
            surface,
            offers: HashMap::new(),
            active_offer: None,
            position: (0, 0),
            deliver: Arc::new(deliver),
        };
        queue
            .roundtrip(&mut state)
            .context("initialize Wayland drag and drop")?;

        Ok(Some(Self {
            _device: device,
            _seat: seat,
            _manager: manager,
            state,
            queue,
            connection,
        }))
    }

    pub fn dispatch_pending(&mut self) -> Result<()> {
        self.queue
            .dispatch_pending(&mut self.state)
            .context("dispatch Wayland drag and drop")?;
        self.connection
            .flush()
            .context("flush Wayland drag and drop requests")?;
        Ok(())
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for DropState {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDeviceManager, ()> for DropState {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataDeviceManager,
        _event: <WlDataDeviceManager as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for DropState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        event: <WlSeat as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Name { name } = event {
            eprintln!("Wayland file-drop receiver using seat {name:?}");
        }
    }
}

impl Dispatch<WlDataOffer, ()> for DropState {
    fn event(
        state: &mut Self,
        offer: &WlDataOffer,
        event: <WlDataOffer as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let offer_state = state
            .offers
            .entry(offer.id().protocol_id())
            .or_insert_with(|| OfferState {
                proxy: offer.clone(),
                mime_types: HashSet::new(),
            });
        if let wl_data_offer::Event::Offer { mime_type } = event {
            offer_state.mime_types.insert(mime_type);
        }
    }
}

impl Dispatch<WlDataDevice, ()> for DropState {
    wayland_client::event_created_child!(DropState, WlDataDevice, [
        0 => (WlDataOffer, ())
    ]);

    fn event(
        state: &mut Self,
        _device: &WlDataDevice,
        event: <WlDataDevice as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                state
                    .offers
                    .entry(id.id().protocol_id())
                    .or_insert_with(|| OfferState {
                        proxy: id,
                        mime_types: HashSet::new(),
                    });
            }
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            } => {
                state.position = (x.round() as i32, y.round() as i32);
                state.active_offer = None;
                if surface.id() != state.surface {
                    return;
                }
                let Some(offer) = id else {
                    return;
                };
                let offer_id = offer.id().protocol_id();
                let accepts_files = state
                    .offers
                    .get(&offer_id)
                    .is_some_and(|offer| offer.mime_types.contains(URI_LIST_MIME));
                if accepts_files {
                    offer.accept(serial, Some(URI_LIST_MIME.to_string()));
                    if offer.version() >= 3 {
                        offer.set_actions(DndAction::Copy, DndAction::Copy);
                    }
                    state.active_offer = Some(offer_id);
                } else {
                    offer.accept(serial, None);
                }
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                state.position = (x.round() as i32, y.round() as i32);
            }
            wl_data_device::Event::Leave => state.active_offer = None,
            wl_data_device::Event::Drop => receive_drop(state),
            wl_data_device::Event::Selection { .. } => {}
            _ => {}
        }
    }
}

fn receive_drop(state: &mut DropState) {
    let Some(offer_id) = state.active_offer.take() else {
        return;
    };
    let Some(offer) = state.offers.remove(&offer_id).map(|offer| offer.proxy) else {
        eprintln!("Wayland file drop lost its data offer");
        return;
    };
    let Ok((read_fd, write_fd)) = pipe_with(PipeFlags::CLOEXEC) else {
        eprintln!("Wayland file drop could not create its transfer pipe");
        return;
    };
    offer.receive(URI_LIST_MIME.to_string(), write_fd.as_fd());
    drop(write_fd);
    if offer.version() >= 3 {
        offer.finish();
    }
    let deliver = Arc::clone(&state.deliver);
    let position = state.position;
    thread::spawn(move || {
        let mut contents = String::new();
        if File::from(read_fd)
            .take(MAX_URI_LIST_BYTES)
            .read_to_string(&mut contents)
            .is_err()
        {
            eprintln!("failed to read Wayland file-drop URI list");
            return;
        }
        for path in parse_uri_list(&contents) {
            deliver(DropEvent { path, position });
        }
    });
}

fn parse_uri_list(contents: &str) -> Vec<PathBuf> {
    contents
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(file_uri_to_path)
        .collect()
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let encoded = if encoded.starts_with('/') {
        encoded
    } else {
        encoded.strip_prefix("localhost")?
    };
    let encoded = if encoded.starts_with('/') {
        encoded
    } else {
        return None;
    };
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = hex_value(bytes[index + 1])?;
            let low = hex_value(bytes[index + 2])?;
            decoded.push(high << 4 | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok().map(PathBuf::from)
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_uri_list;

    #[test]
    fn parses_local_file_uri_lists() {
        let paths =
            parse_uri_list("# files\r\nfile:///tmp/red%20image.png\r\nhttps://example.test/no\r\n");
        assert_eq!(paths, [std::path::PathBuf::from("/tmp/red image.png")]);
    }
}
